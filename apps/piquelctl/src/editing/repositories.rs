//! Editing repository-backed applications and their webhooks.

use super::{EditFlags, RepositoryPathArgs, RepositoryUrlArgs, SyncMode};
use crate::{commands::resolve_application, error::Result, output::Console};
use clap::{Args, Subcommand};
use piqueld_client::{Client, GitRepository, RepositoryManifest, edit::ApplicationEdit};

#[derive(Debug, Args)]
pub(crate) struct RepositoryTarget {
    /// Application name or stable ID.
    app: String,
    #[command(flatten)]
    flags: EditFlags,
}
#[derive(Debug, Subcommand)]
pub(crate) enum RepositoryCommand {
    /// Connect the application to a manifest file in a Git repository.
    ///
    /// Deployments then fetch the manifest from the repository. Every
    /// environment follows `--branch`, as do environments created later
    /// without their own, unless the manifest declares the environment's
    /// branch. Change an environment's branch with `env branch`. You can
    /// still edit the application: deployments of the connection's branch
    /// deploy your edits until a push changes the manifest file.
    Connect {
        #[command(flatten)]
        target: RepositoryTarget,
        /// Git clone URL, or a path on the daemon's host.
        // A distinct id keeps clap from storing this in the global `--url` endpoint.
        #[arg(id = "repository_url", value_name = "URL")]
        url: String,
        /// Path of the manifest file, relative to the repository root.
        path: String,
        /// Branch the environments follow.
        #[arg(long, default_value = "main")]
        branch: String,
        /// Pin this full commit hash instead of following the branch.
        #[arg(long)]
        commit: Option<String>,
        /// Whether to deploy pushes, as `app repository sync` sets it.
        #[arg(
            long = "sync",
            value_name = "MODE",
            value_parser = SyncMode::parser(),
            default_value = "off"
        )]
        sync: SyncMode,
        /// Seconds between polls with `--sync poll`, from 60 to 86400. By
        /// default, 300.
        #[arg(long, value_name = "SECONDS")]
        interval: Option<u32>,
    },
    /// Stop reading the manifest from Git, keeping the saved manifest.
    ///
    /// Edits are kept, and environments that followed a branch deploy the
    /// saved manifest. Declared environments lose their `branch`, `commit`
    /// and `sync`. It
    /// fails while services mount files from the repository: give them
    /// inline content first.
    Disconnect(RepositoryTarget),
    /// Change the repository every environment fetches the manifest from.
    Url(RepositoryUrlArgs),
    /// Change the manifest file every environment reads.
    Path(RepositoryPathArgs),
    /// Choose whether environments and previews deploy pushes.
    ///
    /// While sync is on, every preview, and every environment that opted in
    /// with `env sync` and follows an unpinned branch, redeploys when its
    /// branch moves past the head of its last deployment.
    Sync {
        #[command(flatten)]
        target: RepositoryTarget,
        /// How to detect pushes.
        #[arg(value_parser = SyncMode::parser())]
        mode: SyncMode,
        /// Seconds between polls with `poll`, from 60 to 86400. By default,
        /// 300.
        #[arg(long, value_name = "SECONDS")]
        interval: Option<u32>,
    },
    /// Discard the edits made since the manifest was last read from the
    /// repository, saving that manifest again.
    ///
    /// `app show` lists the edits. Deployments of the connection's branch
    /// then deploy the manifest as read, until a push changes it.
    Reset(RepositoryTarget),
    /// Show the GitHub webhook that reports pushes, or generate its secret.
    Webhook {
        #[command(subcommand)]
        command: WebhookCommand,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum WebhookCommand {
    /// Show the payload URL to configure in GitHub, and when the secret was
    /// generated.
    ///
    /// The URL appears once the daemon sets `ingress.webhook_hostname`.
    Show {
        /// Application name or stable ID.
        app: String,
    },
    /// Generate a new webhook secret, replacing the previous one, and print
    /// it.
    ///
    /// The previous secret stops working at once. The new one is shown only
    /// this time.
    Rotate {
        /// Application name or stable ID.
        app: String,
        /// Skip the confirmation prompt. You need it when not on a terminal.
        #[arg(long, short)]
        yes: bool,
    },
}
impl RepositoryCommand {
    /// Saves a manifest repository connection, disconnection, or single-field change.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let (target, edit) = match self {
            Self::Connect {
                target,
                url,
                path,
                branch,
                commit,
                sync,
                interval,
            } => (
                target,
                ApplicationEdit::Repository(Some(RepositoryManifest {
                    repository: GitRepository {
                        url: url.clone(),
                        branch: branch.clone(),
                        commit: commit.clone(),
                    },
                    path: path.clone(),
                    sync: sync.with_interval(*interval)?,
                })),
            ),
            Self::Sync {
                target,
                mode,
                interval,
            } => (
                target,
                ApplicationEdit::RepositorySync(mode.with_interval(*interval)?),
            ),
            Self::Webhook { command } => return command.run(client, console).await,
            Self::Reset(target) => return target.flags.reset(client, console, &target.app).await,
            Self::Disconnect(target) => (target, ApplicationEdit::Repository(None)),
            Self::Url(args) => (
                &args.target,
                ApplicationEdit::RepositoryUrl(args.value.clone()),
            ),
            Self::Path(args) => (
                &args.target,
                ApplicationEdit::RepositoryPath(args.value.clone()),
            ),
        };
        target.flags.save(client, console, &target.app, &edit).await
    }
}

impl WebhookCommand {
    /// Shows the webhook, or confirms and generates a new secret.
    async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        match self {
            Self::Show { app } => {
                let application = resolve_application(client, app).await?;
                console.emit(
                    &client
                        .webhook(application.application.id().as_str())
                        .await?,
                )
            }
            Self::Rotate { app, yes } => {
                let application = resolve_application(client, app).await?;
                console
                    .confirm(
                        *yes,
                        &format!(
                            "Generate a new webhook secret for application {:?}? The current one stops verifying at once.",
                            application.application.metadata().name
                        ),
                    )
                    .await?;
                let id = application.application.id().as_str();
                console.emit(&client.generate_webhook_secret(id).await?)
            }
        }
    }
}
