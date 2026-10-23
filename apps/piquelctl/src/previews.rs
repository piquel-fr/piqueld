//! `piquelctl preview`: disposable deployments of a branch. A preview is
//! addressed by its stable ID, its slug, or its branch and `--slot`, in that
//! order. Creating, deploying, and deleting one need no application revision.
use crate::{
    cli::LogArgs,
    commands::{resolve_application, wait_for_deletion, wait_for_operation},
    deployments::{Deployed, Target, UrlArgs, WaitArgs, wait_for_accepted},
    editing::{EditFlags, visibility},
    environments::logs,
    error::{CliError, ErrorKind, Result},
    exec::ExecArgs,
    output::{
        Console,
        reports::{CreatedPreviewReport, DeletionReport},
    },
    snapshot::{History, Worktree},
    support::{join_or, retry_transport},
};
use clap::{Args, Subcommand};
use piqueld_client::{
    ApplicationView, BranchState, Client, ClientError, CreatePreviewRequest, CreatedPreview,
    EnvironmentView, PrunePreviewsRequest, Visibility, codes, edit::ApplicationEdit,
};

// `preview` subcommands; `///` on variants and fields is user-facing help.
#[derive(Debug, Subcommand)]
pub(crate) enum PreviewCommand {
    /// Create a preview of a branch and deploy it.
    ///
    /// Only for repository-backed applications. If the preview of that
    /// branch and slot already exists, the command shows it and its latest
    /// deployment without deploying it again. It fails when a new preview
    /// would exceed the daemon's preview limits.
    Create {
        /// Application name or stable ID.
        application: String,
        /// Branch of the manifest repository to deploy.
        #[arg(long)]
        branch: String,
        /// Distinguishes several previews of one branch, such as one per
        /// agent: 1 to 63 lowercase letters, digits, or hyphens, starting
        /// with a letter and ending with a letter or digit.
        #[arg(long)]
        slot: Option<String>,
        /// Return once the daemon accepts the deployment, without waiting for
        /// it to finish.
        #[arg(long)]
        no_wait: bool,
    },
    /// Deploy the head of a preview's branch again, or a snapshot of your Git
    /// worktree with `--local`.
    ///
    /// Deploying the branch's head replaces a local snapshot the preview runs.
    Deploy(DeployArgs),
    /// List an application's previews with their status and branch state.
    List {
        /// Application name or stable ID.
        application: String,
    },
    /// Show one preview, its URLs, and the state of its branch.
    ///
    /// The branch is `exists`, `moved` (past the deployed commit), `gone`,
    /// `local` (the preview runs or is deploying a local snapshot), or
    /// `unknown` (the repository can't be read).
    Show(PreviewArgs),
    /// Cap the visibility of every preview's routes in the saved manifest.
    ///
    /// Each route of a preview gets the stricter of its own visibility and
    /// this cap, saved in the `[spec.previews]` block. Previews apply it on
    /// their next deployment. Repository-backed applications set it in their
    /// manifest instead.
    Visibility(VisibilityArgs),
    /// Print the most recent lines of a preview's Docker logs.
    Logs {
        #[command(flatten)]
        target: PreviewArgs,
        #[command(flatten)]
        window: LogArgs,
    },
    /// Wait until a deployment of a preview is ready.
    ///
    /// The command never follows a newer deployment. It exits with code 3 if
    /// a newer deployment or the preview's deletion supersedes it, 5 if it
    /// fails, and 4 when `--timeout` runs out. Builds usually take longer
    /// than the default `--timeout`.
    Wait(WaitArgs<PreviewArgs>),
    /// Print the URLs of a preview's routes, and whether each is ready.
    Url(UrlArgs<PreviewArgs>),
    /// Run a one-off command in a running task of a preview's service,
    /// streaming its output.
    ///
    /// `piquelctl` exits with the command's exit code. `--timeout` limits
    /// each request, not the session.
    Exec(ExecArgs<PreviewArgs>),
    /// Delete a preview and every volume it created.
    ///
    /// The volumes' data is lost. The command asks for confirmation and
    /// waits until the preview is gone.
    Delete {
        #[command(flatten)]
        target: PreviewArgs,
        #[command(flatten)]
        flags: PreviewDeletionFlags,
    },
    /// Delete the previews whose branch is gone from the repository.
    ///
    /// The command lists them and asks for confirmation. The daemon checks
    /// each branch again and keeps any preview it can no longer confirm is
    /// gone. Previews whose branch state is `unknown` or `local` are kept.
    /// Their volumes are deleted with them.
    Prune {
        /// Application name or stable ID.
        application: String,
        /// Select the previews whose branch no longer exists. Required.
        #[arg(long, required = true)]
        branch_gone: bool,
        #[command(flatten)]
        flags: PreviewDeletionFlags,
    },
}

// What `preview deploy` deploys, and where.
#[derive(Debug, Args)]
pub(crate) struct DeployArgs {
    /// Application name or stable ID.
    application: String,
    /// Preview ID, slug, or branch. Omit it with `--local`.
    #[arg(required_unless_present = "local", conflicts_with = "local")]
    preview: Option<String>,
    /// The preview's slot. Give it to address a preview with a slot by its
    /// branch, or to choose the preview `--local` deploys to.
    #[arg(long)]
    slot: Option<String>,
    /// Deploy a snapshot of the Git worktree of the current directory,
    /// without pushing it.
    ///
    /// The snapshot has the worktree's committed, uncommitted, and untracked
    /// files. Ignored files are left out, or sent as committed when they are
    /// tracked. The command lists the uncommitted files it sends, and deploys
    /// to the preview of the current branch and `--slot`, creating it if
    /// needed. Your repository is never changed. A large first upload may
    /// need a longer `--timeout`.
    #[arg(long)]
    local: bool,
    /// Upload every commit of `HEAD` with the snapshot.
    ///
    /// By default, the command sends only the commits since the nearest
    /// ancestor of `HEAD` that a remote-tracking branch has, and the daemon
    /// fetches that ancestor from the application's repository. Use this
    /// when there is no such ancestor, or the daemon can't fetch it.
    #[arg(long, requires = "local")]
    full_history: bool,
    /// Deploy the snapshot as the preview of this branch instead of the
    /// current one. Required when `HEAD` is detached.
    #[arg(long, requires = "local")]
    branch: Option<String>,
    /// Return once the daemon accepts the deployment, without waiting for it
    /// to finish.
    #[arg(long)]
    no_wait: bool,
}

// `preview visibility` arguments.
#[derive(Debug, Args)]
pub(crate) struct VisibilityArgs {
    /// Application name or stable ID.
    application: String,
    /// The strictest visibility preview routes get. `private`, the default,
    /// keeps every route on the tailnet; `public` restricts nothing.
    #[arg(value_parser = visibility())]
    visibility: Visibility,
    #[command(flatten)]
    flags: EditFlags,
}

impl VisibilityArgs {
    /// Saves the previews' visibility ceiling in `[spec.previews]`.
    async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let application = resolve_application(client, &self.application).await?;
        let edit = ApplicationEdit::PreviewVisibility(self.visibility);
        self.flags
            .save_loaded(client, console, &application, &edit)
            .await
    }
}

// An application and one of its previews.
#[derive(Debug, Args)]
pub(crate) struct PreviewArgs {
    /// Application name or stable ID.
    application: String,
    /// Preview ID, slug, or branch, matched in that order.
    preview: String,
    /// The preview's slot. Needed to address a preview with a slot by its
    /// branch.
    #[arg(long)]
    slot: Option<String>,
}

// Confirmation and waiting flags for preview deletions.
#[derive(Debug, Args)]
pub(crate) struct PreviewDeletionFlags {
    /// Skip the confirmation prompt. You need it when not on a terminal.
    #[arg(long)]
    yes: bool,
    /// Return once the daemon accepts the deletion, without waiting for it
    /// to finish.
    #[arg(long)]
    no_wait: bool,
}

impl PreviewArgs {
    /// Loads the application and selects the preview.
    async fn resolve(&self, client: &Client) -> Result<(ApplicationView, EnvironmentView)> {
        let application = resolve_application(client, &self.application).await?;
        let preview = application
            .preview(&self.preview, self.slot.as_deref())
            .cloned()
            .ok_or_else(|| {
                CliError::new(
                    ErrorKind::Input,
                    format!(
                        "preview {:?} of application {:?} was not found",
                        self.preview,
                        application.application.metadata().name
                    ),
                )
            })?;
        Ok((application, preview))
    }
}

impl Target for PreviewArgs {
    async fn environment(&self, client: &Client) -> Result<EnvironmentView> {
        Ok(self.resolve(client).await?.1)
    }
}

impl PreviewCommand {
    /// Runs a `preview` subcommand.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        match self {
            Self::Create {
                application,
                branch,
                slot,
                no_wait,
            } => {
                let request = CreatePreviewRequest {
                    branch: branch.clone(),
                    slot: slot.clone(),
                };
                create(client, console, application, &request, *no_wait).await
            }
            Self::Deploy(args) => args.run(client, console).await,
            Self::List { application } => {
                let application = resolve_application(client, application).await?;
                let previews = client
                    .previews(application.application.id().as_str())
                    .await?;
                console.emit(&previews)
            }
            Self::Show(target) => {
                let (_, preview) = target.resolve(client).await?;
                console.emit(&client.preview(preview.id.as_str()).await?)
            }
            Self::Visibility(args) => args.run(client, console).await,
            Self::Logs { target, window } => {
                let (_, preview) = target.resolve(client).await?;
                logs(console, client, &preview, window).await
            }
            Self::Wait(args) => args.run(console, client).await,
            Self::Url(args) => args.run(console, client).await,
            Self::Exec(_) => unreachable!("exec sessions run without the command timeout"),
            Self::Delete { target, flags } => delete(client, console, target, flags).await,
            Self::Prune {
                application, flags, ..
            } => prune(client, console, application, flags).await,
        }
    }
}

impl DeployArgs {
    /// Deploys the preview's branch head, or a snapshot of the worktree.
    async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let Some(preview) = self.preview.clone().filter(|_| !self.local) else {
            return self.deploy_local(client, console).await;
        };
        let target = PreviewArgs {
            application: self.application.clone(),
            preview,
            slot: self.slot.clone(),
        };
        let (_, preview) = target.resolve(client).await?;
        let accepted = retry_transport(|| client.deploy_preview(preview.id.as_str())).await?;
        wait_for_accepted(console, client, &preview, self.no_wait, &accepted).await
    }

    /// Snapshots the current worktree (see `snapshot`) within the
    /// daemon's limits, lists the uncommitted files it sends, and uploads
    /// it thin against a base the repository likely has, or with the whole
    /// history with `--full-history`, then waits for its deployment unless
    /// `--no-wait`. The upload is not retried: the daemon may already have
    /// received it.
    async fn deploy_local(&self, client: &Client, console: &mut Console) -> Result<()> {
        let worktree = Worktree::current().await?;
        let branch = match &self.branch {
            Some(branch) => branch.clone(),
            None => worktree.branch().await?.ok_or_else(|| {
                CliError::new(
                    ErrorKind::Input,
                    "HEAD is detached; name the preview's branch with --branch",
                )
            })?,
        };
        let application = resolve_application(client, &self.application).await?;
        let id = application.application.id().as_str();
        // Older daemons report no limits; they still refuse what exceeds them.
        let limits = retry_transport(|| client.system_status()).await?.snapshots;
        let history = if self.full_history {
            History::Full
        } else {
            History::Thin
        };
        let progress = console.start_task("snapshot");
        progress.update("committing the worktree");
        let packed = async {
            let snapshot = worktree.snapshot(&branch, limits.as_ref()).await?;
            let bundle = snapshot.bundle(history).await?;
            bundle.check_size(limits.as_ref())?;
            Ok((snapshot, bundle))
        }
        .await;
        let (snapshot, bundle) = progress.settle(packed, |(_, bundle)| bundle.describe())?;
        if !snapshot.uncommitted.is_empty() {
            const LISTED: usize = 20;
            let listed = snapshot.uncommitted.len().min(LISTED);
            let files = snapshot.uncommitted[..listed].join(", ");
            let more = match snapshot.uncommitted.len() - listed {
                0 => String::new(),
                more => format!(" and {more} more"),
            };
            console.info(format_args!("sending uncommitted files: {files}{more}"))?;
        }
        let progress = console.start_task("upload");
        progress.update("uploading");
        let uploaded = async {
            let file = tokio::fs::File::open(&bundle.path)
                .await
                .map_err(|error| CliError::new(ErrorKind::General, error.to_string()))?;
            client
                .deploy_local_preview(id, (&branch, self.slot.as_deref()), (file, bundle.length))
                .await
                .map_err(|error| match error {
                    ClientError::Api { error, .. }
                        if error.code == codes::SNAPSHOT_BASE_UNAVAILABLE =>
                    {
                        CliError::new(
                            ErrorKind::Input,
                            format!(
                                "the daemon could not fetch {} from the application's repository; \
                                 push it, or pass --full-history to upload the whole history",
                                bundle.snapshot.base.as_deref().unwrap_or("the base"),
                            ),
                        )
                    }
                    error => error.into(),
                })
        }
        .await;
        let created = progress.settle(uploaded, |_| "uploaded".to_owned())?;
        let mut deployed = deployed(console, client, &created, self.no_wait).await?;
        // A deployment read back once it ended names its snapshot itself.
        if self.no_wait {
            deployed = deployed.snapshot(&branch, &bundle.snapshot);
        }
        console.emit(&CreatedPreviewReport {
            created: &created,
            deployed,
        })
    }
}

/// Creates a preview, or finds the existing one of its branch and slot, then
/// waits for its deployment unless `--no-wait`.
async fn create(
    client: &Client,
    console: &mut Console,
    application: &str,
    request: &CreatePreviewRequest,
    no_wait: bool,
) -> Result<()> {
    let application = resolve_application(client, application).await?;
    let id = application.application.id().as_str();
    let created = retry_transport(|| client.create_preview(id, request)).await?;
    let deployed = deployed(console, client, &created, no_wait).await?;
    console.emit(&CreatedPreviewReport {
        created: &created,
        deployed,
    })
}

/// The deployment that created or deployed a preview: as accepted with
/// `no_wait`, else read back once it ended.
async fn deployed(
    console: &mut Console,
    client: &Client,
    created: &CreatedPreview,
    no_wait: bool,
) -> Result<Deployed> {
    let operation_id = &created.operation.operation_id;
    if no_wait {
        return Ok(Deployed::accepted(&created.preview, operation_id));
    }
    let operation = wait_for_operation(console, client, operation_id).await?;
    Deployed::read(client, &created.preview, operation).await
}

/// Confirms and deletes a preview and its volumes. Waits until it is gone
/// unless `--no-wait`, polling the environment endpoint, which unlike the
/// preview one never reads the repository.
async fn delete(
    client: &Client,
    console: &mut Console,
    target: &PreviewArgs,
    flags: &PreviewDeletionFlags,
) -> Result<()> {
    let (application, preview) = target.resolve(client).await?;
    console
        .confirm(
            flags.yes,
            &format!(
                "Delete preview {:?} of application {:?}? Its volumes and their data will be removed.",
                preview.name.as_str(),
                application.application.metadata().name
            ),
        )
        .await?;
    let accepted = retry_transport(|| client.delete_preview(preview.id.as_str())).await?;
    if flags.no_wait {
        return console.emit(&DeletionReport::accepted(&accepted).removing_volumes());
    }
    wait_for_deletion(
        console,
        client,
        || client.environment(preview.id.as_str()),
        std::slice::from_ref(&accepted.operation_id),
    )
    .await?;
    console.emit(&DeletionReport::completed(&accepted).removing_volumes())
}

/// Lists the previews whose branch is gone, confirms, and asks the daemon to
/// delete them. The daemon checks each branch again and keeps any it can no
/// longer confirm gone. Waits for every deletion unless `--no-wait`.
async fn prune(
    client: &Client,
    console: &mut Console,
    application: &str,
    flags: &PreviewDeletionFlags,
) -> Result<()> {
    let application = resolve_application(client, application).await?;
    let id = application.application.id().as_str();
    let previews = client.previews(id).await?;
    for view in &previews {
        if let BranchState::Unknown { message } = &view.branch {
            console.warning(format_args!(
                "{}: branch state unknown ({message}); keeping it",
                view.preview.name
            ))?;
        }
    }
    let gone = previews
        .iter()
        .filter(|view| view.branch == BranchState::Gone)
        .map(|view| &view.preview)
        .collect::<Vec<_>>();
    if gone.is_empty() {
        return console.emit(&Vec::<piqueld_client::DeletedPreview>::new());
    }
    let names = join_or(gone.iter().map(|preview| &preview.name), "");
    console
        .confirm(
            flags.yes,
            &format!(
                "Delete previews {names} of application {:?}, whose branches are gone? Their volumes and their data will be removed.",
                application.application.metadata().name
            ),
        )
        .await?;
    let request = PrunePreviewsRequest {
        previews: gone.iter().map(|preview| preview.id.clone()).collect(),
    };
    let deleted = client.prune_previews(id, &request).await?;
    for preview in &gone {
        if !deleted
            .iter()
            .any(|deleted| deleted.preview.id == preview.id)
        {
            console.warning(format_args!(
                "{}: its branch is no longer confirmed gone; keeping it",
                preview.name
            ))?;
        }
    }
    if !flags.no_wait {
        for deleted in &deleted {
            wait_for_deletion(
                console,
                client,
                || client.environment(deleted.preview.id.as_str()),
                std::slice::from_ref(&deleted.operation.operation_id),
            )
            .await?;
        }
    }
    console.emit(&deleted)
}
