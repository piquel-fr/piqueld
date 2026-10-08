//! `piquelctl preview`: disposable deployments of a branch. A preview is
//! addressed by its stable ID, its slug, or its branch and `--slot`, in that
//! order. Creating, deploying, and deleting one need no application revision.
use crate::{
    cli::{Cli, LogArgs},
    commands::{resolve_application, wait_for_accepted, wait_for_deletion, wait_for_operation},
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
    ApplicationView, BranchState, Client, CreatePreviewRequest, EnvironmentView,
    PrunePreviewsRequest,
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
    /// Deploy the head of a preview's branch again.
    Deploy {
        #[command(flatten)]
        target: PreviewArgs,
        /// Return after the daemon accepts the deployment.
        #[arg(long)]
        no_wait: bool,
    },
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
            Self::Deploy { target, no_wait } => {
                let (_, preview) = target.resolve(client).await?;
                let accepted =
                    retry_transport(|| client.deploy_preview(preview.id.as_str())).await?;
                wait_for_accepted(console, client, *no_wait, &accepted).await
            }
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
            Self::Delete { target, flags } => delete(cli, client, console, target, flags).await,
            Self::Prune {
                application, flags, ..
            } => prune(cli, client, console, application, flags).await,
        }
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
    let outcome = if no_wait {
        None
    } else {
        let operation = wait_for_operation(console, client, &created.operation.operation_id);
        Some(operation.await?.state)
    };
    console.emit(&CreatedPreviewReport {
        created: &created,
        outcome,
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
