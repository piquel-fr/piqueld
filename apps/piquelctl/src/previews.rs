//! `piquelctl preview`: disposable deployments of a branch. A preview is
//! addressed by its stable ID, its slug, or its branch and `--slot`, in that
//! order. Creating, deploying, and deleting one need no application revision.
use crate::output::TaskOutcome;
use crate::snapshot::Worktree;
use crate::{
    cli::{Cli, LogArgs},
    commands::{resolve_application, wait_for_deletion, wait_for_operation},
    deployments::{Deployed, Target, UrlArgs, WaitArgs, wait_for_accepted},
    environments::logs,
    error::{CliError, ErrorKind, Result},
    output::{
        Console,
        reports::{CreatedPreviewReport, DeletionReport},
    },
    support::{confirm, retry_transport},
};
use clap::{Args, Subcommand};
use piqueld_client::{
    ApplicationView, BranchState, Client, ClientError, CreatePreviewRequest, EnvironmentView,
    PrunePreviewsRequest, codes,
};

// `preview` subcommands; `///` on variants and fields is user-facing help.
#[derive(Debug, Subcommand)]
pub(crate) enum PreviewCommand {
    /// Create and deploy a preview of a branch of the manifest repository. For
    /// an existing preview of that branch and slot, show it without redeploying.
    Create {
        /// Application name or stable ID.
        application: String,
        /// Branch to deploy.
        #[arg(long)]
        branch: String,
        /// Distinguishes several previews of one branch, e.g. one per agent.
        #[arg(long)]
        slot: Option<String>,
        /// Return after the daemon accepts the deployment.
        #[arg(long)]
        no_wait: bool,
    },
    /// Deploy the head of a preview's branch again, or with `--local` a
    /// snapshot of the current Git worktree.
    Deploy(DeployArgs),
    /// List an application's previews with their status and branch state.
    List {
        /// Application name or stable ID.
        application: String,
    },
    /// Show one preview, its URLs, and whether its branch exists, moved, or is gone.
    Show(PreviewArgs),
    /// Read a bounded snapshot of a preview's Docker logs.
    Logs {
        #[command(flatten)]
        target: PreviewArgs,
        #[command(flatten)]
        window: LogArgs,
    },
    /// Wait until a deployment of a preview is ready. Exits 3 if a newer
    /// deployment supersedes it, 5 if it fails, and 4 at `--timeout`.
    Wait(WaitArgs<PreviewArgs>),
    /// Print the URLs of a preview's routes, and whether each is ready.
    Url(UrlArgs<PreviewArgs>),
    /// Confirm and delete a preview with every volume it ever created.
    Delete {
        #[command(flatten)]
        target: PreviewArgs,
        #[command(flatten)]
        flags: PreviewDeletionFlags,
    },
    /// Confirm and delete the previews whose branch the repository confirms
    /// is gone. Previews whose branch state is unknown are kept.
    Prune {
        /// Application name or stable ID.
        application: String,
        /// Select previews whose branch no longer exists.
        #[arg(long, required = true)]
        branch_gone: bool,
        #[command(flatten)]
        flags: PreviewDeletionFlags,
    },
}

/// What `preview deploy` deploys, and where.
#[derive(Debug, Args)]
pub(crate) struct DeployArgs {
    /// Application name or stable ID.
    application: String,
    /// Preview ID, slug, or branch. With `--local`, the preview of the
    /// current branch is used instead.
    #[arg(required_unless_present = "local", conflicts_with = "local")]
    preview: Option<String>,
    /// The preview's slot, when addressing it by branch or with `--local`.
    #[arg(long)]
    slot: Option<String>,
    /// Deploy a snapshot of the current Git worktree: its committed,
    /// uncommitted, and untracked files, except ignored ones. Creates the
    /// preview of its branch and `--slot` if needed. Never changes the
    /// repository.
    #[arg(long)]
    local: bool,
    /// The branch the snapshot is deployed as, instead of the current one;
    /// required when HEAD is detached.
    #[arg(long, requires = "local")]
    branch: Option<String>,
    /// Return after the daemon accepts the deployment.
    #[arg(long)]
    no_wait: bool,
}

/// An application and one of its previews.
#[derive(Debug, Args)]
pub(crate) struct PreviewArgs {
    /// Application name or stable ID.
    application: String,
    /// Preview ID, slug, or branch.
    preview: String,
    /// The preview's slot, when addressing it by branch.
    #[arg(long)]
    slot: Option<String>,
}

// Confirmation and waiting flags for preview deletions.
#[derive(Debug, Args)]
pub(crate) struct PreviewDeletionFlags {
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    yes: bool,
    /// Return after the daemon accepts the deletion.
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
    pub(crate) async fn run(
        &self,
        cli: &Cli,
        client: &Client,
        console: &mut Console,
    ) -> Result<()> {
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
            Self::Logs { target, window } => {
                let (_, preview) = target.resolve(client).await?;
                logs(console, client, &preview, window).await
            }
            Self::Wait(args) => args.run(console, client).await,
            Self::Url(args) => args.run(console, client).await,
            Self::Delete { target, flags } => delete(cli, client, console, target, flags).await,
            Self::Prune {
                application, flags, ..
            } => prune(cli, client, console, application, flags).await,
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

    /// Snapshots the current worktree (see `snapshot`), uploads it thin
    /// against a base the repository likely has, or complete when the
    /// daemon cannot fetch that base, then waits for its deployment unless
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
        let progress = console.start_task("snapshot");
        let uploaded = async {
            progress.update("committing the worktree");
            let snapshot = worktree.snapshot(&branch).await?;
            let mut bundle = snapshot.bundle(true).await?;
            loop {
                progress.update(&bundle.describe());
                let file = tokio::fs::File::open(&bundle.path)
                    .await
                    .map_err(|error| CliError::new(ErrorKind::General, error.to_string()))?;
                let upload = client
                    .deploy_local_preview(
                        id,
                        (&branch, self.slot.as_deref()),
                        (file, bundle.length),
                    )
                    .await;
                match upload {
                    Err(ClientError::Api { error, .. })
                        if error.code == codes::SNAPSHOT_BASE_UNAVAILABLE
                            && bundle.snapshot.base.is_some() =>
                    {
                        bundle = snapshot.bundle(false).await?;
                    }
                    upload => return Ok::<_, CliError>((upload?, bundle.snapshot)),
                }
            }
        }
        .await;
        let (created, snapshot) = match uploaded {
            Ok(uploaded) => {
                progress.finish(TaskOutcome::Succeeded, "uploaded");
                uploaded
            }
            Err(error) => {
                progress.finish(TaskOutcome::Failed, &error.to_string());
                return Err(error);
            }
        };
        let deployed = if self.no_wait {
            Deployed::accepted(&created.preview, &created.operation.operation_id)
                .snapshot(&branch, &snapshot)
        } else {
            let operation =
                wait_for_operation(console, client, &created.operation.operation_id).await?;
            Deployed::read(client, &created.preview, operation).await?
        };
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
    let deployed = if no_wait {
        Deployed::accepted(&created.preview, &created.operation.operation_id)
    } else {
        let operation =
            wait_for_operation(console, client, &created.operation.operation_id).await?;
        Deployed::read(client, &created.preview, operation).await?
    };
    console.emit(&CreatedPreviewReport {
        created: &created,
        deployed,
    })
}

/// Confirms and deletes a preview and its volumes. Waits until it is gone
/// unless `--no-wait`, polling the environment endpoint, which unlike the
/// preview one never reads the repository.
async fn delete(
    cli: &Cli,
    client: &Client,
    console: &mut Console,
    target: &PreviewArgs,
    flags: &PreviewDeletionFlags,
) -> Result<()> {
    let (application, preview) = target.resolve(client).await?;
    confirm(
        console,
        cli.noninteractive,
        flags.yes,
        &format!(
            "Delete preview {:?} of application {:?}? Its volumes and their data will be removed. [y/N] ",
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
    cli: &Cli,
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
    let names = gone
        .iter()
        .map(|preview| preview.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    confirm(
        console,
        cli.noninteractive,
        flags.yes,
        &format!(
            "Delete previews {names} of application {:?}, whose branches are gone? Their volumes and their data will be removed. [y/N] ",
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
