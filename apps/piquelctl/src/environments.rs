//! `piquelctl env`: an application's environments, plus the environment
//! selection and runtime commands shared with `app deploy`, `app reconcile`,
//! and `app logs`.
use crate::{
    cli::{Cli, DeletionFlags, LogArgs, OperationFlags, RevisionArgs, expected_generation},
    commands::{resolve_application, wait_for_deletion, wait_for_operation},
    deployments::{Target, UrlArgs, WaitArgs, wait_for_accepted},
    editing::{EditFlags, on_off, visibility},
    error::{CliError, ErrorKind, ErrorReport, Result},
    output::{
        Console,
        reports::{DeletionReport, EnvironmentRow, EnvironmentShowReport, Named, PromotionReport},
    },
    support::{join_or, retry_transport},
};
use clap::{Args, Subcommand};
use piqueld_client::{
    ApplicationView, Client, ClientError, CreateEnvironmentRequest, EnvironmentBranchRequest,
    EnvironmentRequest, EnvironmentSourceRequest, EnvironmentStatusView, EnvironmentView,
    PromoteRequest, Visibility, edit::ApplicationEdit,
};

// `env` subcommands; `///` on variants and fields is user-facing help.
#[derive(Debug, Subcommand)]
pub(crate) enum EnvCommand {
    /// List an application's environments and their status.
    ///
    /// Previews are not listed; use `preview list` for them.
    List {
        /// Application name or stable ID.
        application: String,
    },
    /// Add an environment to an application.
    ///
    /// Nothing is deployed until you run `env deploy`, or `env promote` for a
    /// promoted environment. The environment deploys the application's saved
    /// manifest or, when the application is repository-backed, the manifest
    /// on its own branch.
    ///
    /// You can't create an environment that the saved manifest has a
    /// `[spec.environments.<name>]` block for: saving the manifest creates it.
    Create(CreateArgs),
    /// Make an environment receive releases promoted from another one, or
    /// build its own source again.
    ///
    /// Nothing is deployed. A promoted environment never builds: it deploys
    /// what `env promote` sends it. When the saved manifest sets the
    /// environment's `promote_from`, `branch`, `commit` or `sync`, change it
    /// there instead.
    Source(SourceArgs),
    /// Deploy a release into a promoted environment without building it.
    ///
    /// By default, it deploys the release of the source environment's current
    /// deployment, which must have succeeded and be healthy now. The command
    /// asks for confirmation and waits for the deployment.
    Promote(PromoteArgs),
    /// Point an environment at another branch of the manifest repository, or
    /// pin or unpin its commit.
    ///
    /// Only for repository-backed applications. Nothing is deployed: the
    /// environment's next deployment fetches the manifest from the new branch
    /// or commit. A promoted environment has no branch; use `env source
    /// --tracking` first. When the saved manifest sets the environment's
    /// `branch`, `commit`, `promote_from` or `sync`, change it there instead.
    Branch(BranchArgs),
    /// Choose whether pushes to an environment's branch deploy it.
    ///
    /// This applies while the application's repository syncs (`app repository
    /// sync`). An environment that opts in deploys again whenever its branch
    /// moves past the head of its last deployment; one never deployed starts
    /// following from its first deployment. A pinned commit never follows
    /// pushes. When the saved manifest sets the environment's `sync`,
    /// `branch`, `commit` or `promote_from`, change it there instead. Opting
    /// in while the repository syncs needs the `apps:deploy` permission too.
    Sync(SyncArgs),
    /// Grant an environment the daemon's telemetry, or revoke it.
    ///
    /// The receiving service of a granted environment, marked with `app
    /// service telemetry`, gets the traces, metrics and logs of every
    /// application. Revoking stops the export at once. Either way, the daemon
    /// mounts or unmounts the telemetry socket within seconds, which restarts
    /// the service; nothing is deployed. Both need the `telemetry:grant`
    /// permission. Previews are never granted.
    Telemetry(TelemetryArgs),
    /// Show one environment, the manifest it deploys, and its status.
    Show(EnvironmentArgs),
    /// Cap the visibility of an environment's routes in the saved manifest.
    ///
    /// Each route gets the stricter of its own visibility and this cap. The
    /// cap is saved in the environment's `[spec.environments.<name>]` block.
    /// `private` adds the block when the manifest has none for the
    /// environment; `public`, which is no cap, only changes an existing one.
    Visibility(VisibilityArgs),
    /// Rename an environment without redeploying it.
    ///
    /// The command fails while the manifest the environment deploys, or the
    /// application's saved manifest, has a `[spec.environments.<name>]`
    /// block for the old or the new name.
    Rename(RenameArgs),
    /// Delete an environment, keeping its named volumes.
    ///
    /// The command removes the environment's services and network, after
    /// confirmation, and waits until it is gone. To delete an environment
    /// that the saved manifest declares, remove its `[spec.environments.<name>]`
    /// block first.
    Delete(DeleteArgs),
    /// Deploy an environment from its source.
    ///
    /// For a repository-backed application, it fetches the manifest from the
    /// environment's branch, or from `--branch` or `--commit` for this
    /// deployment only. Otherwise, it deploys the saved manifest. The command
    /// asks for confirmation and waits for the deployment. Promoted
    /// environments deploy with `env promote` instead.
    Deploy {
        #[command(flatten)]
        target: EnvironmentArgs,
        #[command(flatten)]
        flags: OperationFlags,
        #[command(flatten)]
        revision: RevisionArgs,
    },
    /// Retry an environment's latest operation with the inputs it was given.
    ///
    /// If the operation is still in progress, the command waits for it
    /// instead of starting it again.
    Reconcile {
        #[command(flatten)]
        target: EnvironmentArgs,
        #[command(flatten)]
        flags: OperationFlags,
    },
    /// List, regenerate, or delete an environment's generated secrets.
    Secret {
        /// Application name or stable ID.
        application: String,
        /// Environment name or stable ID. You can omit it when the
        /// application has exactly one environment.
        #[arg(long = "env", value_name = "ENV")]
        environment: Option<String>,
        #[command(subcommand)]
        action: crate::secrets::GeneratedSecretAction,
    },
    /// Print the most recent lines of an environment's Docker logs.
    Logs {
        #[command(flatten)]
        target: EnvironmentArgs,
        #[command(flatten)]
        window: LogArgs,
    },
    /// Wait until a deployment of an environment is ready.
    ///
    /// The command never follows a newer deployment. It exits with code 3 if
    /// a newer deployment or the environment's deletion supersedes it, 5 if
    /// it fails, and 4 when `--timeout` runs out. Builds usually take longer
    /// than the default `--timeout`.
    Wait(WaitArgs<EnvironmentArgs>),
    /// Print the URLs of an environment's routes, and whether each is ready.
    Url(UrlArgs<EnvironmentArgs>),
}

// An application and, when it has several, one of its environments.
#[derive(Debug, Args)]
pub(crate) struct EnvironmentArgs {
    /// Application name or stable ID.
    pub(crate) application: String,
    /// Environment name or stable ID. You can omit it when the application
    /// has exactly one environment.
    pub(crate) environment: Option<String>,
}

// An application and one of its environments, named.
#[derive(Debug, Args)]
pub(crate) struct NamedEnvironment {
    /// Application name or stable ID.
    application: String,
    /// Environment name or stable ID.
    environment: String,
}

// Precondition and confirmation flags for environment lifecycle changes.
#[derive(Debug, Args)]
pub(crate) struct ChangeFlags {
    /// Change the environment only if the application's generation is still
    /// `GENERATION`. By default, it must still be the generation the command
    /// read, so a concurrent change fails the command.
    #[arg(long, value_name = "GENERATION")]
    expected_generation: Option<u64>,
    /// Skip the confirmation prompt. You need it when not on a terminal.
    #[arg(long)]
    yes: bool,
    /// Change the environment even if the application changed since the
    /// command read it. It still asks for confirmation.
    #[arg(long, conflicts_with = "expected_generation")]
    force: bool,
}

impl ChangeFlags {
    /// The inspected revision a change is conditioned on, unless forced.
    fn expected(&self, application: &ApplicationView) -> Option<u64> {
        expected_generation(self.force, self.expected_generation, application.generation)
    }
}

impl EnvCommand {
    /// Runs an `env` subcommand.
    pub(crate) async fn run(
        &self,
        cli: &Cli,
        client: &Client,
        console: &mut Console,
    ) -> Result<()> {
        match self {
            Self::List { application } => list(cli, client, console, application).await,
            Self::Create(args) => args.run(client, console).await,
            Self::Source(args) => args.run(client, console).await,
            Self::Promote(args) => args.run(client, console).await,
            Self::Branch(args) => args.run(client, console).await,
            Self::Sync(args) => args.run(client, console).await,
            Self::Telemetry(args) => args.run(client, console).await,
            Self::Show(target) => show(client, console, target).await,
            Self::Visibility(args) => args.run(client, console).await,
            Self::Secret {
                application,
                environment,
                action,
            } => {
                action
                    .run(client, console, application, environment.as_deref())
                    .await
            }
            Self::Rename(args) => args.run(client, console).await,
            Self::Delete(args) => args.run(client, console).await,
            Self::Deploy {
                target,
                flags,
                revision,
            } => {
                let (application, environment) = target.resolve(client).await?;
                deploy(client, console, &application, &environment, flags, revision).await
            }
            Self::Reconcile { target, flags } => {
                let (application, environment) = target.resolve(client).await?;
                reconcile(client, console, &application, &environment, flags).await
            }
            Self::Logs { target, window } => {
                logs(console, client, &target.resolve(client).await?.1, window).await
            }
            Self::Wait(args) => args.run(console, client).await,
            Self::Url(args) => args.run(console, client).await,
        }
    }
}

// `env sync` arguments.
#[derive(Debug, Args)]
pub(crate) struct SyncArgs {
    #[command(flatten)]
    target: NamedEnvironment,
    /// `on` deploys the environment when its branch moves; `off` deploys it
    /// only when you ask.
    #[arg(value_name = "on|off", action = clap::ArgAction::Set, value_parser = on_off())]
    enabled: bool,
}

impl SyncArgs {
    /// Opts the environment in or out of its application's sync.
    async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let (application, environment) = self.target.resolve(client).await?;
        let id = environment.id.as_str();
        let environment = retry_transport(|| client.set_environment_sync(id, self.enabled)).await?;
        console.emit(&Named::new(&environment, &application))
    }
}

// `env telemetry` arguments.
#[derive(Debug, Args)]
pub(crate) struct TelemetryArgs {
    #[command(flatten)]
    target: NamedEnvironment,
    /// `on` grants the environment every application's telemetry; `off`
    /// revokes it.
    #[arg(value_name = "on|off", action = clap::ArgAction::Set, value_parser = on_off())]
    granted: bool,
}

impl TelemetryArgs {
    /// Grants or revokes the environment's telemetry.
    async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let (application, environment) = self.target.resolve(client).await?;
        let id = environment.id.as_str();
        let environment =
            retry_transport(|| client.set_environment_telemetry(id, self.granted)).await?;
        console.emit(&Named::new(&environment, &application))
    }
}

// `env visibility` arguments.
#[derive(Debug, Args)]
pub(crate) struct VisibilityArgs {
    #[command(flatten)]
    target: NamedEnvironment,
    /// The strictest visibility the environment's routes get. `private` keeps
    /// every route on the tailnet; `public`, the default, restricts nothing.
    #[arg(value_parser = visibility())]
    visibility: Visibility,
    #[command(flatten)]
    flags: EditFlags,
}

impl VisibilityArgs {
    /// Saves the environment's visibility ceiling in
    /// `[spec.environments.<name>]`, addressing it by name or ID.
    async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let (application, environment) = self.target.resolve(client).await?;
        let edit = ApplicationEdit::EnvironmentVisibility {
            environment: environment.name.to_string(),
            visibility: self.visibility,
        };
        self.flags
            .save_loaded(client, console, &application, &edit)
            .await
    }
}

/// Lists an application's environments with their status.
async fn list(cli: &Cli, client: &Client, console: &mut Console, application: &str) -> Result<()> {
    let application = resolve_application(client, application).await?;
    let statuses = statuses(client, &application.environments).await;
    let rows = rows(
        cli,
        console,
        application.application.metadata().name.as_str(),
        &application.environments,
        statuses,
    )?;
    console.emit(&rows)
}

// `env create` arguments.
#[derive(Debug, Args)]
pub(crate) struct CreateArgs {
    /// Application name or stable ID.
    application: String,
    /// Name of the new environment, unique within the application: 1 to 63
    /// lowercase letters, digits, or hyphens, starting with a letter and
    /// ending with a letter or digit.
    name: String,
    /// Branch of the manifest repository the environment follows. By default,
    /// the branch `spec.manifest` names. Only for repository-backed
    /// applications.
    #[arg(long)]
    branch: Option<String>,
    /// Deploy this commit of `--branch` instead of the branch's head: a full
    /// commit hash of 40 or 64 lowercase hexadecimal characters.
    #[arg(long, value_name = "SHA", requires = "branch")]
    commit: Option<String>,
    /// Make the environment promoted: it never builds, and only receives
    /// releases promoted from this environment of the application, given by
    /// name or stable ID, with `env promote`.
    #[arg(long, value_name = "ENV", conflicts_with = "branch")]
    promote_from: Option<String>,
    #[command(flatten)]
    change: ChangeFlags,
}

impl CreateArgs {
    /// Confirms and adds the environment, conditioned on the inspected
    /// application revision.
    async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let application = resolve_application(client, &self.application).await?;
        let request = CreateEnvironmentRequest {
            name: self.name.clone(),
            branch: self.branch.clone(),
            commit: self.commit.clone(),
            promote_from: self
                .promote_from
                .as_deref()
                .map(|source| promotion_source(&application, source))
                .transpose()?,
            expected_generation: self.change.expected(&application),
        };
        console
            .confirm(
                self.change.yes,
                &format!(
                    "Create environment {:?} of application {:?}?",
                    request.name,
                    application.application.metadata().name
                ),
            )
            .await?;
        let environment = retry_transport(|| {
            client.create_environment(
                application.application.id().as_str(),
                &request,
                self.change.force,
            )
        })
        .await?;
        console.emit(&Named::new(&environment, &application))
    }
}

// `env branch` arguments.
#[derive(Debug, Args)]
pub(crate) struct BranchArgs {
    #[command(flatten)]
    target: NamedEnvironment,
    /// Branch of the manifest repository the environment follows.
    branch: String,
    /// Deploy this commit of the branch instead of the branch's head: a full
    /// commit hash of 40 or 64 lowercase hexadecimal characters. Without it,
    /// the environment follows the branch's head, unpinning any commit.
    #[arg(long, value_name = "SHA")]
    commit: Option<String>,
    #[command(flatten)]
    change: ChangeFlags,
}

impl BranchArgs {
    /// Confirms and points an environment at another branch, conditioned on the
    /// inspected application revision. Nothing is redeployed.
    async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let (application, environment) = self.target.resolve(client).await?;
        console
            .confirm(
                self.change.yes,
                &format!(
                    "Point environment {:?} of application {:?} at branch {:?}?",
                    environment.name.as_str(),
                    application.application.metadata().name,
                    self.branch
                ),
            )
            .await?;
        let request = EnvironmentBranchRequest {
            branch: self.branch.clone(),
            commit: self.commit.clone(),
            expected_generation: self.change.expected(&application),
        };
        let environment = retry_transport(|| {
            client.set_environment_branch(environment.id.as_str(), &request, self.change.force)
        })
        .await?;
        console.emit(&Named::new(&environment, &application))
    }
}

/// The ID of `source`, an environment of `application` named or identified
/// as a promotion source.
fn promotion_source(application: &ApplicationView, source: &str) -> Result<String> {
    application
        .environment(source)
        .map(|environment| environment.id.to_string())
        .ok_or_else(|| {
            CliError::new(
                ErrorKind::Input,
                format!(
                    "environment {source:?} of application {:?} was not found",
                    application.application.metadata().name
                ),
            )
        })
}

// `env source` arguments.
#[derive(Debug, Args)]
pub(crate) struct SourceArgs {
    #[command(flatten)]
    target: NamedEnvironment,
    /// Receive releases promoted from this environment of the application,
    /// given by name or stable ID.
    #[arg(long, value_name = "ENV", required_unless_present = "tracking")]
    promote_from: Option<String>,
    /// Stop receiving promoted releases and build again: the saved manifest,
    /// or the branch `spec.manifest` names. Use `env branch` afterwards to
    /// follow another branch.
    #[arg(long, conflicts_with = "promote_from")]
    tracking: bool,
    #[command(flatten)]
    change: ChangeFlags,
}

impl SourceArgs {
    /// Confirms and changes where the environment's releases come from,
    /// conditioned on the inspected application revision.
    async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let (application, environment) = self.target.resolve(client).await?;
        let promote_from = self
            .promote_from
            .as_deref()
            .map(|source| promotion_source(&application, source))
            .transpose()?;
        let change = match &self.promote_from {
            Some(source) => format!("receive releases promoted from {source:?}"),
            None => "build from its own source again".into(),
        };
        console
            .confirm(
                self.change.yes,
                &format!(
                    "Make environment {:?} of application {:?} {change}?",
                    environment.name.as_str(),
                    application.application.metadata().name,
                ),
            )
            .await?;
        let request = EnvironmentSourceRequest {
            promote_from,
            expected_generation: self.change.expected(&application),
        };
        let environment = retry_transport(|| {
            client.set_environment_source(environment.id.as_str(), &request, self.change.force)
        })
        .await?;
        console.emit(&Named::new(&environment, &application))
    }
}

// `env promote` arguments.
#[derive(Debug, Args)]
pub(crate) struct PromoteArgs {
    /// Application name or stable ID.
    application: String,
    /// Name or stable ID of the promoted environment to deploy.
    environment: String,
    /// Promote the release of this deployment of the source environment. The
    /// command fails if it is no longer the source's current deployment.
    #[arg(long, value_name = "ID")]
    deployment: Option<String>,
    /// Deploy this earlier release of the application instead, with the
    /// environment's current secrets.
    #[arg(long, value_name = "ID", conflicts_with = "deployment")]
    release: Option<String>,
    /// Show what would be deployed, and any secret it can't use, without
    /// deploying. The command exits with code 3 if the promotion would be
    /// refused. With `--release`, you can plan for any environment, promoted
    /// or not.
    #[arg(long)]
    plan: bool,
    #[command(flatten)]
    flags: OperationFlags,
}

impl PromoteArgs {
    /// Plans, or confirms and promotes, the selected release, guarded by the
    /// expected generation, defaulting to the current one. Waits unless
    /// `--no-wait`. A plan listing unusable secrets fails as a conflict,
    /// since promoting would.
    async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let (application, environment) =
            select(client, &self.application, Some(&self.environment)).await?;
        let mut request = PromoteRequest {
            deployment: self.deployment.clone(),
            release: self.release.clone(),
            expected_generation: None,
        };
        let id = environment.id.as_str();
        if self.plan {
            let plan = client.plan_promotion(id, &request).await?;
            console.emit(&Named::new(&plan, &application))?;
            if plan.plan.is_blocked() {
                return Err(CliError::blocked_plan(&plan));
            }
            if let Some(release) = plan.release.as_ref().filter(|r| !r.secrets.is_empty()) {
                return Err(CliError::new(
                    ErrorKind::Conflict,
                    format!(
                        "{} secret(s) are missing or not allowed for environment {}",
                        release.secrets.len(),
                        environment.name
                    ),
                )
                .with_details(serde_json::json!({"secrets": release.secrets})));
            }
            return Ok(());
        }
        let what = match (&self.release, &self.deployment) {
            (Some(release), _) => format!("release {release}"),
            (None, Some(deployment)) => format!("deployment {deployment}'s release"),
            (None, None) => "the source's current release".into(),
        };
        console
            .confirm(
                self.flags.yes,
                &format!(
                    "Promote {what} into environment {:?} of application {:?}?",
                    environment.name.as_str(),
                    application.application.metadata().name
                ),
            )
            .await?;
        request.expected_generation = Some(
            self.flags
                .expected_generation
                .unwrap_or(application.generation),
        );
        let accepted = retry_transport(|| client.promote_environment(id, &request, false)).await?;
        let report = PromotionReport::new(&application, &accepted);
        if self.flags.no_wait {
            return console.emit(&report);
        }
        let operation =
            wait_for_operation(console, client, &accepted.operation.operation_id).await?;
        console.emit(&report.finished(&operation))
    }
}

/// Shows one environment, the manifest it deploys, and its status, warning
/// with any status message.
async fn show(client: &Client, console: &mut Console, target: &EnvironmentArgs) -> Result<()> {
    let (application, environment) = target.resolve(client).await?;
    let detail = client.environment_detail(environment.id.as_str()).await?;
    let stored = client
        .stored_secrets(application.application.id().as_str())
        .await?;
    console.emit(&EnvironmentShowReport {
        detail: &detail,
        stored: &stored,
    })?;
    if let Some(message) = &detail.status.message {
        console.warning(message)?;
    }
    Ok(())
}

// `env rename` arguments.
#[derive(Debug, Args)]
pub(crate) struct RenameArgs {
    #[command(flatten)]
    target: NamedEnvironment,
    /// New environment name, unique within the application: 1 to 63
    /// lowercase letters, digits, or hyphens, starting with a letter and
    /// ending with a letter or digit.
    new_name: String,
    #[command(flatten)]
    change: ChangeFlags,
}

impl RenameArgs {
    /// Confirms and renames an environment without redeploying it.
    async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let (application, environment) = self.target.resolve(client).await?;
        let new_name = &self.new_name;
        console
            .confirm(
                self.change.yes,
                &format!(
                    "Rename environment {:?} of application {:?} to {new_name:?}?",
                    environment.name.as_str(),
                    application.application.metadata().name
                ),
            )
            .await?;
        let request = EnvironmentRequest {
            name: new_name.clone(),
            expected_generation: self.change.expected(&application),
        };
        let environment = retry_transport(|| {
            client.rename_environment(environment.id.as_str(), &request, self.change.force)
        })
        .await?;
        console.emit(&Named::new(&environment, &application))
    }
}

// `env delete` arguments.
#[derive(Debug, Args)]
pub(crate) struct DeleteArgs {
    #[command(flatten)]
    target: NamedEnvironment,
    #[command(flatten)]
    deletion: DeletionFlags,
}

impl DeleteArgs {
    /// Confirms and deletes an environment (named volumes are retained), guarded by
    /// the expected generation unless `--force`. Waits until the environment is gone
    /// unless `--no-wait`.
    async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let (application, environment) = self.target.resolve(client).await?;
        let flags = &self.deletion;
        let name = application.application.metadata().name.clone();
        console.info(format_args!(
            "deleting environment {} ({}) of {name}: managed services and network are removed; named volumes are retained",
            environment.name.as_str(),
            environment.id
        ))?;
        console
            .confirm(
                flags.yes,
                &format!(
                    "Delete environment {:?} of application {name:?}? Named volumes will be retained.",
                    environment.name.as_str()
                ),
            )
            .await?;
        let accepted = retry_transport(|| {
            client.delete_environment(
                environment.id.as_str(),
                expected_generation(
                    flags.force,
                    flags.expected_generation,
                    application.generation,
                ),
                flags.force,
            )
        })
        .await?;
        if flags.no_wait {
            return console.emit(&DeletionReport::accepted(&accepted));
        }
        wait_for_deletion(
            console,
            client,
            || client.environment(environment.id.as_str()),
            std::slice::from_ref(&accepted.operation_id),
        )
        .await?;
        console.emit(&DeletionReport::completed(&accepted))
    }
}

impl EnvironmentArgs {
    /// Loads the application and selects the named or only environment.
    async fn resolve(&self, client: &Client) -> Result<(ApplicationView, EnvironmentView)> {
        select(client, &self.application, self.environment.as_deref()).await
    }
}

impl Target for EnvironmentArgs {
    async fn environment(&self, client: &Client) -> Result<EnvironmentView> {
        Ok(self.resolve(client).await?.1)
    }
}

impl NamedEnvironment {
    /// Loads the application and selects the named environment.
    async fn resolve(&self, client: &Client) -> Result<(ApplicationView, EnvironmentView)> {
        select(client, &self.application, Some(&self.environment)).await
    }
}

/// Loads an application and selects one environment: the named one, or the
/// only one when `environment` is omitted. Never picks one of several silently.
pub(crate) async fn select(
    client: &Client,
    application: &str,
    environment: Option<&str>,
) -> Result<(ApplicationView, EnvironmentView)> {
    let application = resolve_application(client, application).await?;
    let name = application.application.metadata().name.clone();
    let selected = match environment {
        Some(environment) => application.environment(environment).cloned().ok_or_else(|| {
            CliError::new(
                ErrorKind::Input,
                format!("environment {environment:?} of application {name:?} was not found"),
            )
        })?,
        None => application
            .sole_environment()
            .cloned()
            .map_err(|environments| {
                let message = if environments.is_empty() {
                    format!(
                        "application {name:?} has no environments; create one with `piquelctl env create {name} <NAME>`"
                    )
                } else {
                    format!(
                        "environment required: application {name:?} has environments {}; use `piquelctl env <COMMAND> {name} <ENV>`",
                        join_or(environments, "")
                    )
                };
                CliError::new(ErrorKind::Input, message)
            })?,
    };
    Ok((application, selected))
}

/// Fetches every environment's status concurrently, in environment order.
pub(crate) async fn statuses(
    client: &Client,
    environments: &[EnvironmentView],
) -> Vec<std::result::Result<EnvironmentStatusView, ClientError>> {
    futures_util::future::join_all(
        environments
            .iter()
            .map(|environment| client.environment_status(environment.id.as_str())),
    )
    .await
}

/// Pairs environments with their fetched statuses, warning with each status
/// message. A failed status request becomes a warning and an `unavailable`
/// row instead of hiding the other environments.
pub(crate) fn rows(
    cli: &Cli,
    console: &mut Console,
    application: &str,
    environments: &[EnvironmentView],
    statuses: Vec<std::result::Result<EnvironmentStatusView, ClientError>>,
) -> Result<Vec<EnvironmentRow>> {
    let mut rows = Vec::with_capacity(environments.len());
    for (environment, status) in environments.iter().zip(statuses) {
        let label = format!("{application}/{}", environment.name);
        let status = match status {
            Ok(status) => {
                if let Some(message) = &status.message {
                    console.warning(format_args!("{label}: {message}"))?;
                }
                Some(status)
            }
            Err(error) => {
                console.warning_report(&ErrorReport::warning(
                    &CliError::from(error),
                    cli,
                    &label,
                ))?;
                None
            }
        };
        rows.push(EnvironmentRow {
            environment: environment.clone(),
            status,
        });
    }
    Ok(rows)
}

/// Confirms and deploys an environment from its source, fetching the manifest
/// from its branch (or `--branch` or `--commit`, for this deployment only) and
/// resolving sources again. Guarded by the expected generation, defaulting to
/// the current one. Waits unless `--no-wait`, then warns about anything the
/// deployment reported, such as an ignored `spec.manifest`.
pub(crate) async fn deploy(
    client: &Client,
    console: &mut Console,
    application: &ApplicationView,
    environment: &EnvironmentView,
    flags: &OperationFlags,
    revision: &RevisionArgs,
) -> Result<()> {
    console
        .confirm(
            flags.yes,
            &format!(
                "Deploy application {:?} to environment {:?}?",
                application.application.metadata().name,
                environment.name.as_str()
            ),
        )
        .await?;
    let revision = revision.revision();
    let accepted = retry_transport(|| {
        client.deploy_environment(
            environment.id.as_str(),
            flags.expected_generation.unwrap_or(application.generation),
            revision.as_ref(),
        )
    })
    .await?;
    let result = wait_for_accepted(console, client, environment, flags.no_wait, &accepted).await;
    // The outcome matters more than its warnings, so failing to read them is ignored.
    if !flags.no_wait
        && let Ok(deployments) = client.deployments(environment.id.as_str(), None).await
    {
        let warnings = deployments
            .items
            .into_iter()
            .filter(|deployment| deployment.operation.id == accepted.operation_id)
            .flat_map(|deployment| deployment.warnings);
        for warning in warnings {
            console.warning(format_args!("{}: {}", warning.code, warning.message))?;
        }
    }
    result
}

/// Confirms and retries an environment's latest operation with its saved inputs,
/// guarded by the expected generation only when one is given. Waits for it
/// unless `--no-wait`.
pub(crate) async fn reconcile(
    client: &Client,
    console: &mut Console,
    application: &ApplicationView,
    environment: &EnvironmentView,
    flags: &OperationFlags,
) -> Result<()> {
    console
        .confirm(
            flags.yes,
            &format!(
                "Retry the latest operation of application {:?} in environment {:?}?",
                application.application.metadata().name,
                environment.name.as_str()
            ),
        )
        .await?;
    let accepted = retry_transport(|| {
        client.reconcile_environment(environment.id.as_str(), flags.expected_generation)
    })
    .await?;
    wait_for_accepted(console, client, environment, flags.no_wait, &accepted).await
}

/// Emits a bounded snapshot of runtime logs, warning when the daemon truncated it.
pub(crate) async fn logs(
    console: &mut Console,
    client: &Client,
    environment: &EnvironmentView,
    window: &LogArgs,
) -> Result<()> {
    let logs = client
        .environment_logs(
            environment.id.as_str(),
            window.service.as_deref(),
            window.tail,
            window.since_seconds,
        )
        .await?;
    console.emit(&logs)?;
    if logs.truncated {
        console.warning("Log snapshot was truncated; narrow the service or time window.")?;
    }
    Ok(())
}
