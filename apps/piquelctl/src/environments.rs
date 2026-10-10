//! `piquelctl env`: an application's environments, plus the environment
//! selection and runtime commands shared with `app deploy`, `app reconcile`,
//! and `app logs`.
use crate::{
    cli::{Cli, DeletionFlags, LogArgs, OperationFlags, RevisionArgs},
    commands::{resolve_application, wait_for_accepted, wait_for_deletion, wait_for_operation},
    editing::{EditFlags, save_loaded, visibility},
    error::{CliError, ErrorKind, ErrorReport, Result},
    output::{
        Console,
        reports::{DeletionReport, EnvironmentRow, EnvironmentShowReport, PromotionReport},
    },
    support::{confirm, retry_transport},
};
use clap::{Args, Subcommand, builder::TypedValueParser};
use piqueld_client::{
    ApplicationView, Client, ClientError, CreateEnvironmentRequest, EnvironmentBranchRequest,
    EnvironmentRequest, EnvironmentSourceRequest, EnvironmentStatusView, EnvironmentView,
    PromoteRequest, Visibility, edit::ApplicationEdit,
};

// `env` subcommands; `///` on variants and fields is user-facing help.
#[derive(Debug, Subcommand)]
pub(crate) enum EnvCommand {
    /// List an application's environments and their status.
    List {
        /// Application name or stable ID.
        application: String,
    },
    /// Add an environment. It deploys the application's saved manifest or, when
    /// the application is repository-backed, the manifest on its own branch.
    Create(CreateArgs),
    /// Make an environment receive releases promoted from another one instead
    /// of building, or track its own source again. Nothing is deployed.
    Source(SourceArgs),
    /// Deploy a release into a promoted environment without building: by
    /// default the one its source environment currently runs.
    Promote(PromoteArgs),
    /// Point an environment of a repository-backed application at another
    /// branch, or pin or unpin its commit. Its next deployment fetches it.
    Branch {
        /// Application name or stable ID.
        application: String,
        /// Environment name or stable ID.
        environment: String,
        /// Branch of the manifest repository to follow.
        branch: String,
        /// Pin this full commit instead of following the branch head; omit to unpin.
        #[arg(long)]
        commit: Option<String>,
        #[command(flatten)]
        change: ChangeFlags,
    },
    /// Opt an environment into or out of its application's sync: while it is
    /// in, pushes to its branch deploy it, from its next deployment if it was
    /// never deployed. Pinned environments never sync.
    Sync(SyncArgs),
    /// Show one environment and its status.
    Show(EnvironmentArgs),
    /// Cap the visibility of an environment's routes in the saved manifest:
    /// `private` keeps every route on the tailnet, `public` restricts nothing.
    Visibility(VisibilityArgs),
    /// Rename an environment without redeploying it.
    Rename {
        /// Application name or stable ID.
        application: String,
        /// Environment name or stable ID.
        environment: String,
        /// New environment name, unique within the application.
        new_name: String,
        #[command(flatten)]
        change: ChangeFlags,
    },
    /// Confirm and delete an environment; named volumes are retained.
    Delete {
        /// Application name or stable ID.
        application: String,
        /// Environment name or stable ID.
        environment: String,
        #[command(flatten)]
        deletion: DeletionFlags,
    },
    /// Deploy the application's saved configuration to an environment, fetching
    /// the manifest and resolving sources again.
    Deploy {
        #[command(flatten)]
        target: EnvironmentArgs,
        #[command(flatten)]
        flags: OperationFlags,
        #[command(flatten)]
        revision: RevisionArgs,
    },
    /// Retry an environment's latest operation once it has ended or failed,
    /// reusing its saved inputs.
    Reconcile {
        #[command(flatten)]
        target: EnvironmentArgs,
        #[command(flatten)]
        flags: OperationFlags,
    },
    /// List or delete an environment's generated secrets.
    Secret {
        /// Application name or stable ID.
        application: String,
        /// Environment name or stable ID; optional when the application has exactly one.
        #[arg(long = "env", value_name = "ENV")]
        environment: Option<String>,
        #[command(subcommand)]
        action: crate::secrets::GeneratedSecretAction,
    },
    /// Read a bounded snapshot of an environment's Docker logs.
    Logs {
        #[command(flatten)]
        target: EnvironmentArgs,
        #[command(flatten)]
        window: LogArgs,
    },
}

/// An application and, when it has several, one of its environments.
#[derive(Debug, Args)]
pub(crate) struct EnvironmentArgs {
    /// Application name or stable ID.
    pub(crate) application: String,
    /// Environment name or stable ID; optional when the application has exactly one.
    pub(crate) environment: Option<String>,
}

// Precondition and confirmation flags for environment lifecycle changes.
#[derive(Debug, Args)]
pub(crate) struct ChangeFlags {
    /// Require this application revision.
    #[arg(long)]
    expected_generation: Option<u64>,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    yes: bool,
    /// Override the revision precondition (does not skip confirmation).
    #[arg(long, conflicts_with = "expected_generation")]
    force: bool,
}

impl ChangeFlags {
    /// The inspected revision a change is conditioned on, unless forced.
    fn expected(&self, application: &ApplicationView) -> Option<u64> {
        (!self.force).then_some(self.expected_generation.unwrap_or(application.generation))
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
            Self::Create(args) => args.run(cli, client, console).await,
            Self::Source(args) => args.run(cli, client, console).await,
            Self::Promote(args) => args.run(cli, client, console).await,
            Self::Branch {
                application,
                environment,
                branch,
                commit,
                change,
            } => {
                let request = EnvironmentBranchRequest {
                    branch: branch.clone(),
                    commit: commit.clone(),
                    expected_generation: None,
                };
                set_branch(
                    cli,
                    client,
                    console,
                    (application, environment),
                    request,
                    change,
                )
                .await
            }
            Self::Sync(args) => args.run(client, console).await,
            Self::Show(target) => show(client, console, target).await,
            Self::Visibility(args) => args.run(cli, client, console).await,
            Self::Secret {
                application,
                environment,
                action,
            } => {
                action
                    .run(cli, client, console, application, environment.as_deref())
                    .await
            }
            Self::Rename {
                application,
                environment,
                new_name,
                change,
            } => {
                rename(
                    cli,
                    client,
                    console,
                    (application, environment),
                    new_name,
                    change,
                )
                .await
            }
            Self::Delete {
                application,
                environment,
                deletion,
            } => delete(cli, client, console, application, environment, deletion).await,
            Self::Deploy {
                target,
                flags,
                revision,
            } => {
                let (application, environment) = target.resolve(client).await?;
                deploy(
                    cli,
                    client,
                    console,
                    (&application, &environment),
                    flags,
                    revision,
                )
                .await
            }
            Self::Reconcile { target, flags } => {
                let (application, environment) = target.resolve(client).await?;
                reconcile(cli, client, console, (&application, &environment), flags).await
            }
            Self::Logs { target, window } => {
                let (_, environment) = target.resolve(client).await?;
                logs(console, client, &environment, window).await
            }
        }
    }
}

/// `env sync` arguments.
#[derive(Debug, Args)]
pub(crate) struct SyncArgs {
    /// Application name or stable ID.
    application: String,
    /// Environment name or stable ID.
    environment: String,
    /// `on` deploys pushes; `off` deploys only when asked.
    #[arg(
        value_name = "on|off",
        action = clap::ArgAction::Set,
        value_parser = clap::builder::PossibleValuesParser::new(["on", "off"]).map(|value| value == "on")
    )]
    enabled: bool,
}

impl SyncArgs {
    /// Opts the environment in or out of its application's sync.
    async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        let (_, environment) = select(client, &self.application, Some(&self.environment)).await?;
        let id = environment.id.as_str();
        let environment = retry_transport(|| client.set_environment_sync(id, self.enabled)).await?;
        console.emit(&environment)
    }
}

/// `env visibility` arguments.
#[derive(Debug, Args)]
pub(crate) struct VisibilityArgs {
    /// Application name or stable ID.
    application: String,
    /// Environment name or stable ID.
    environment: String,
    /// The strictest visibility its routes get.
    #[arg(value_parser = visibility())]
    visibility: Visibility,
    #[command(flatten)]
    flags: EditFlags,
}

impl VisibilityArgs {
    /// Saves the environment's visibility ceiling in
    /// `[spec.environments.<name>]`, addressing it by name or ID.
    async fn run(&self, cli: &Cli, client: &Client, console: &mut Console) -> Result<()> {
        let (application, environment) =
            select(client, &self.application, Some(&self.environment)).await?;
        let edit = ApplicationEdit::EnvironmentVisibility {
            environment: environment.name.to_string(),
            visibility: self.visibility,
        };
        save_loaded(cli, client, console, application, &self.flags, &edit).await
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

/// `env create` arguments.
#[derive(Debug, Args)]
pub(crate) struct CreateArgs {
    /// Application name or stable ID.
    application: String,
    /// New environment name, unique within the application.
    name: String,
    /// Branch of the manifest repository to follow; defaults to the one
    /// `spec.manifest` names.
    #[arg(long)]
    branch: Option<String>,
    /// Pin this full commit instead of following the branch head.
    #[arg(long, requires = "branch")]
    commit: Option<String>,
    /// Never build: only receive releases promoted from this environment
    /// (name or stable ID) with `env promote`.
    #[arg(long, value_name = "ENV", conflicts_with = "branch")]
    promote_from: Option<String>,
    #[command(flatten)]
    change: ChangeFlags,
}

impl CreateArgs {
    /// Confirms and adds the environment, conditioned on the inspected
    /// application revision.
    async fn run(&self, cli: &Cli, client: &Client, console: &mut Console) -> Result<()> {
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
        confirm(
            console,
            cli.noninteractive,
            self.change.yes,
            &format!(
                "Create environment {:?} of application {:?}? [y/N] ",
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
        console.emit(&environment)
    }
}

/// Confirms and points an environment at another branch, conditioned on the
/// inspected application revision. Nothing is redeployed.
async fn set_branch(
    cli: &Cli,
    client: &Client,
    console: &mut Console,
    (application, environment): (&str, &str),
    mut request: EnvironmentBranchRequest,
    change: &ChangeFlags,
) -> Result<()> {
    let (application, environment) = select(client, application, Some(environment)).await?;
    confirm(
        console,
        cli.noninteractive,
        change.yes,
        &format!(
            "Point environment {:?} of application {:?} at branch {:?}? [y/N] ",
            environment.name.as_str(),
            application.application.metadata().name,
            request.branch
        ),
    )
    .await?;
    request.expected_generation = change.expected(&application);
    let environment = retry_transport(|| {
        client.set_environment_branch(environment.id.as_str(), &request, change.force)
    })
    .await?;
    console.emit(&environment)
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

/// `env source` arguments.
#[derive(Debug, Args)]
pub(crate) struct SourceArgs {
    /// Application name or stable ID.
    application: String,
    /// Environment name or stable ID.
    environment: String,
    /// Receive releases promoted from this environment (name or stable ID).
    #[arg(long, value_name = "ENV", required_unless_present = "tracking")]
    promote_from: Option<String>,
    /// Stop promoting: build the saved manifest, or the branch `spec.manifest`
    /// names, again. `env branch` picks another branch.
    #[arg(long, conflicts_with = "promote_from")]
    tracking: bool,
    #[command(flatten)]
    change: ChangeFlags,
}

impl SourceArgs {
    /// Confirms and changes where the environment's releases come from,
    /// conditioned on the inspected application revision.
    async fn run(&self, cli: &Cli, client: &Client, console: &mut Console) -> Result<()> {
        let (application, environment) =
            select(client, &self.application, Some(&self.environment)).await?;
        let promote_from = self
            .promote_from
            .as_deref()
            .map(|source| promotion_source(&application, source))
            .transpose()?;
        let change = match &self.promote_from {
            Some(source) => format!("receive releases promoted from {source:?}"),
            None => "build from its own source again".into(),
        };
        confirm(
            console,
            cli.noninteractive,
            self.change.yes,
            &format!(
                "Make environment {:?} of application {:?} {change}? [y/N] ",
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
        console.emit(&environment)
    }
}

/// `env promote` arguments.
#[derive(Debug, Args)]
pub(crate) struct PromoteArgs {
    /// Application name or stable ID.
    application: String,
    /// Promoted environment name or stable ID.
    environment: String,
    /// Promote this deployment of the source; refused once it is no longer the
    /// source's current one.
    #[arg(long, value_name = "ID")]
    deployment: Option<String>,
    /// Deploy this earlier release instead, with the environment's current secrets.
    #[arg(long, value_name = "ID", conflicts_with = "deployment")]
    release: Option<String>,
    /// Show what would be deployed, and what is missing, without deploying.
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
    async fn run(&self, cli: &Cli, client: &Client, console: &mut Console) -> Result<()> {
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
            console.emit(&plan)?;
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
        confirm(
            console,
            cli.noninteractive,
            self.flags.yes,
            &format!(
                "Promote {what} into environment {:?} of application {:?}? [y/N] ",
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

/// Confirms and renames an environment without redeploying it.
async fn rename(
    cli: &Cli,
    client: &Client,
    console: &mut Console,
    (application, environment): (&str, &str),
    new_name: &str,
    change: &ChangeFlags,
) -> Result<()> {
    let (application, environment) = select(client, application, Some(environment)).await?;
    confirm(
        console,
        cli.noninteractive,
        change.yes,
        &format!(
            "Rename environment {:?} of application {:?} to {new_name:?}? [y/N] ",
            environment.name.as_str(),
            application.application.metadata().name
        ),
    )
    .await?;
    let request = EnvironmentRequest {
        name: new_name.to_owned(),
        expected_generation: change.expected(&application),
    };
    let environment = retry_transport(|| {
        client.rename_environment(environment.id.as_str(), &request, change.force)
    })
    .await?;
    console.emit(&environment)
}

impl EnvironmentArgs {
    /// Loads the application and selects the named or only environment.
    async fn resolve(&self, client: &Client) -> Result<(ApplicationView, EnvironmentView)> {
        select(client, &self.application, self.environment.as_deref()).await
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
                    let names = environments
                        .iter()
                        .map(piqueld_client::EnvironmentName::as_str)
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!(
                        "environment required: application {name:?} has environments {names}; use `piquelctl env <COMMAND> {name} <ENV>`"
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
    cli: &Cli,
    client: &Client,
    console: &mut Console,
    (application, environment): (&ApplicationView, &EnvironmentView),
    flags: &OperationFlags,
    revision: &RevisionArgs,
) -> Result<()> {
    confirm(
        console,
        cli.noninteractive,
        flags.yes,
        &format!(
            "Deploy application {:?} to environment {:?}? [y/N] ",
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
    let result = wait_for_accepted(console, client, flags.no_wait, &accepted).await;
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
    cli: &Cli,
    client: &Client,
    console: &mut Console,
    (application, environment): (&ApplicationView, &EnvironmentView),
    flags: &OperationFlags,
) -> Result<()> {
    confirm(
        console,
        cli.noninteractive,
        flags.yes,
        &format!(
            "Retry the latest operation of application {:?} in environment {:?}? [y/N] ",
            application.application.metadata().name,
            environment.name.as_str()
        ),
    )
    .await?;
    let accepted = retry_transport(|| {
        client.reconcile_environment(environment.id.as_str(), flags.expected_generation)
    })
    .await?;
    wait_for_accepted(console, client, flags.no_wait, &accepted).await
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

/// Confirms and deletes an environment (named volumes are retained), guarded by
/// the expected generation unless `--force`. Waits until the environment is gone
/// unless `--no-wait`.
async fn delete(
    cli: &Cli,
    client: &Client,
    console: &mut Console,
    application: &str,
    environment: &str,
    flags: &DeletionFlags,
) -> Result<()> {
    let (application, environment) = select(client, application, Some(environment)).await?;
    let name = application.application.metadata().name.clone();
    console.info(format_args!(
        "deleting environment {} ({}) of {name}: managed services and network are removed; named volumes are retained",
        environment.name.as_str(),
        environment.id
    ))?;
    confirm(
        console,
        cli.noninteractive,
        flags.yes,
        &format!(
            "Delete environment {:?} of application {name:?}? Named volumes will be retained. [y/N] ",
            environment.name.as_str()
        ),
    )
    .await?;
    let accepted = retry_transport(|| {
        client.delete_environment(
            environment.id.as_str(),
            (!flags.force).then_some(flags.expected_generation.unwrap_or(application.generation)),
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
