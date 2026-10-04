//! `piquelctl env`: an application's environments, plus the environment
//! selection and runtime commands shared with `app deploy`, `app reconcile`,
//! and `app logs`.
use crate::{
    cli::{Cli, DeletionFlags, LogArgs, OperationFlags, RevisionArgs},
    commands::{resolve_application, wait_for_accepted, wait_for_deletion},
    error::{CliError, ErrorKind, ErrorReport, Result},
    output::{
        Console,
        reports::{DeletionReport, EnvironmentRow, EnvironmentShowReport},
    },
    support::{confirm, retry_transport},
};
use clap::{Args, Subcommand};
use piqueld_client::{
    ApplicationView, Client, ClientError, EnvironmentRequest, EnvironmentStatusView,
    EnvironmentView,
};

// `env` subcommands; `///` on variants and fields is user-facing help.
#[derive(Debug, Subcommand)]
pub(crate) enum EnvCommand {
    /// List an application's environments and their status.
    List {
        /// Application name or stable ID.
        application: String,
    },
    /// Add an environment that deploys the application's saved manifest.
    Create {
        /// Application name or stable ID.
        application: String,
        /// New environment name, unique within the application.
        name: String,
        #[command(flatten)]
        change: ChangeFlags,
    },
    /// Show one environment and its status.
    Show(EnvironmentArgs),
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
    /// The request for `name`, conditioned on the inspected revision unless forced.
    fn request(&self, name: &str, application: &ApplicationView) -> EnvironmentRequest {
        EnvironmentRequest {
            name: name.to_owned(),
            expected_generation: (!self.force)
                .then_some(self.expected_generation.unwrap_or(application.generation)),
        }
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
            Self::Create {
                application,
                name,
                change,
            } => create(cli, client, console, application, name, change).await,
            Self::Show(target) => show(client, console, target).await,
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

/// Confirms and adds an environment, conditioned on the inspected application revision.
async fn create(
    cli: &Cli,
    client: &Client,
    console: &mut Console,
    application: &str,
    name: &str,
    change: &ChangeFlags,
) -> Result<()> {
    let application = resolve_application(client, application).await?;
    confirm(
        console,
        cli.noninteractive,
        change.yes,
        &format!(
            "Create environment {name:?} of application {:?}? [y/N] ",
            application.application.metadata().name
        ),
    )
    .await?;
    let request = change.request(name, &application);
    let environment = retry_transport(|| {
        client.create_environment(
            application.application.id().as_str(),
            &request,
            change.force,
        )
    })
    .await?;
    console.emit(&environment)
}

/// Shows one environment and its status, warning with any status message.
async fn show(client: &Client, console: &mut Console, target: &EnvironmentArgs) -> Result<()> {
    let (application, environment) = target.resolve(client).await?;
    let status = client.environment_status(environment.id.as_str()).await?;
    console.emit(&EnvironmentShowReport {
        application: &application,
        environment: &environment,
        status: &status,
    })?;
    if let Some(message) = &status.message {
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
    let request = change.request(new_name, &application);
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

/// Confirms and deploys saved configuration to an environment, fetching the
/// manifest and resolving sources again (from `--branch` or `--commit` for this
/// deployment only). Guarded by the expected generation, defaulting to the
/// current one. Waits unless `--no-wait`.
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
    wait_for_accepted(console, client, flags.no_wait, &accepted).await
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
