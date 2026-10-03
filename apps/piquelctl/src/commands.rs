//! Connected command implementations plus shared application lookup and operation waiting.
use crate::{
    cli::{
        AppCommand, ApplyArgs, BuildCommand, Cli, Command, DeleteArgs, DeployArgs, ManifestArgs,
        OperationArgs, TargetArgs,
    },
    error::{CliError, ErrorKind, ErrorReport, Result},
    output::{
        Console, TaskOutcome,
        reports::{
            ApplicationRow, DeletionReport, OperationOutcomeReport, SavedDeploymentReport,
            ShowReport, StatusReport, ValidManifestReport,
        },
    },
    support::{confirm, looks_like_application_id, manifest_name, read_manifest, retry_transport},
};
use futures_util::StreamExt;
use piqueld_client::{
    AcceptedOperation, ApplicationSummary, ApplicationView, Client, ClientError,
    ListApplicationsOptions, Operation, OperationState, Page,
};
use serde_json::json;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};
use tokio::time;

use crate::support::{DEFAULT_SOCKET, PAGE_SIZE, POLL_INTERVAL, transport_description};

/// Dispatches every connected command. `login` and `profiles` never reach here:
/// `main` handles them before the timeout supervisor starts.
pub(crate) async fn run(cli: &Cli, client: &Client, console: &mut Console) -> Result<()> {
    match &cli.command {
        Command::Login => unreachable!("login has its own interactive deadline"),
        Command::Logout => crate::auth::logout(cli, client, console).await,
        Command::Whoami => console.emit(&crate::auth::AccountReport(client.auth_me().await?)),
        Command::SetupLink { open } => crate::auth::setup_link(cli, client, console, *open).await,
        Command::Profiles => unreachable!("profiles are listed before connecting"),
        Command::Secrets { action } => action.run(cli, client, console).await,
        Command::Status => status(cli, client, console).await,
        Command::App { command } => app(cli, client, console, command).await,
        Command::Builds(args) => match &args.command {
            BuildCommand::List {
                application,
                cursor,
            } => builds(console, client, application.as_deref(), cursor.as_deref()).await,
            BuildCommand::Logs { id, before } => build_logs(console, client, *id, *before).await,
        },
        Command::Operation(args) => operation(console, client, args).await,
        Command::Events {
            application,
            cursor,
            limit,
        } => {
            let page = client
                .events(application.as_deref(), cursor.as_deref(), *limit)
                .await?;
            console.emit(&page)
        }
    }
}

/// Dispatches `piquelctl app` subcommands.
async fn app(
    cli: &Cli,
    client: &Client,
    console: &mut Console,
    command: &AppCommand,
) -> Result<()> {
    match command {
        AppCommand::List => list(cli, client, console).await,
        AppCommand::Show { name_or_id } => show(console, client, name_or_id).await,
        AppCommand::Logs {
            name_or_id,
            service,
            tail,
            since_seconds,
        } => {
            logs(
                console,
                client,
                name_or_id,
                service.as_deref(),
                *tail,
                *since_seconds,
            )
            .await
        }
        AppCommand::Secret {
            application,
            action,
        } => action.run(cli, client, console, application).await,
        AppCommand::Validate { .. } => unreachable!("validation runs before connecting"),
        AppCommand::Plan(args) => plan_command(console, client, args).await,
        AppCommand::Apply(args) => apply(cli, client, console, args).await,
        AppCommand::Delete(args) => delete(cli, client, console, args).await,
        AppCommand::Reconcile(args) => reconcile(cli, client, console, args).await,
        AppCommand::Rename(args) => {
            crate::editing::save(
                cli,
                client,
                console,
                &args.name_or_id,
                &args.edit,
                &piqueld_client::edit::ApplicationEdit::Name(args.new_name.clone()),
            )
            .await
        }
        AppCommand::Deploy(args) => deploy(cli, client, console, args).await,
        AppCommand::Create(args) => crate::editing::create(cli, client, console, args).await,
        AppCommand::Service { command } => command.run(cli, client, console).await,
        AppCommand::Volume { command } => command.run(cli, client, console).await,
        AppCommand::Route { command } => command.run(cli, client, console).await,
        AppCommand::Repository { command } => command.run(cli, client, console).await,
        AppCommand::Manifest { name_or_id } => {
            let app = resolve_application(client, name_or_id).await?;
            let manifest = client
                .application_manifest(app.application.id().as_str())
                .await?;
            console.emit(&crate::output::reports::ManifestReport(manifest))
        }
    }
}

/// Emits one page of build attempts, optionally filtered by application.
async fn builds(
    console: &mut Console,
    client: &Client,
    application: Option<&str>,
    cursor: Option<&str>,
) -> Result<()> {
    console.emit(&client.builds(application, cursor).await?)
}

/// Emits one page of build output, then warns about expired or truncated output
/// and hints the `--before` offset for the previous page.
async fn build_logs(
    console: &mut Console,
    client: &Client,
    id: i64,
    before: Option<i64>,
) -> Result<()> {
    let page = client.build_logs(id, before, None).await?;
    console.emit(&page)?;
    if page.expired {
        console.warning("Build output has expired.")?;
    }
    if page.truncated {
        console.warning("Build output was truncated at the configured byte limit.")?;
    }
    if let Some(before) = page.previous_offset {
        console.info(format_args!("Load older output with --before {before}"))?;
    }
    Ok(())
}

/// Builds the API client from resolved connection settings: TCP for `--url`, else the
/// Unix socket (default path if unset), plus insecure-HTTP opt-in, saved or
/// `PIQUELD_TOKEN` credentials, the per-request timeout, and a fresh request ID.
/// Fails on invalid endpoint input before any request is sent.
pub(crate) fn build_client(cli: &Cli) -> Result<Client> {
    let client = if let Some(url) = &cli.url {
        Client::tcp(url).map_err(CliError::from)?
    } else {
        Client::unix(
            cli.socket
                .clone()
                .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET)),
        )
    };
    let client = if cli.auth.allow_insecure_http {
        client.with_insecure_http()
    } else {
        client
    };
    let client = crate::auth::Credentials::attach(cli, client)?;
    Ok(client
        .with_timeout(cli.timeout)
        .with_request_id(uuid::Uuid::now_v7().to_string()))
}

/// Reports daemon status together with the transport that was used.
async fn status(cli: &Cli, client: &Client, console: &mut Console) -> Result<()> {
    let status = client.system_status().await?;
    console.emit(&StatusReport {
        status: &status,
        transport: &transport_description(cli),
    })
}

/// Lists every application with its status. Statuses are fetched with up to 8
/// requests in flight while preserving list order; a failed status fetch becomes
/// a per-application warning and an `unavailable` row instead of failing the list.
async fn list(cli: &Cli, client: &Client, console: &mut Console) -> Result<()> {
    let applications = all_applications(client).await?;
    let mut statuses =
        futures_util::stream::iter(applications.into_iter().map(|application| async move {
            let status = client.application_status(application.id.as_str()).await;
            (application, status)
        }))
        .buffered(8);
    let mut items = Vec::new();
    while let Some((application, status)) = statuses.next().await {
        let status = match status {
            Ok(status) => {
                if let Some(message) = &status.message {
                    console.warning(format_args!("{}: {message}", application.name))?;
                }
                Some(status)
            }
            Err(error) => {
                let error = CliError::from(error);
                console.warning_report(&ErrorReport::warning(
                    &error,
                    cli,
                    application.name.as_str(),
                ))?;
                None
            }
        };
        items.push(ApplicationRow {
            application,
            status,
        });
    }
    console.emit(&Page {
        items,
        next_cursor: None,
    })
}

/// Shows one application and its status, warning with any status message.
async fn show(console: &mut Console, client: &Client, name_or_id: &str) -> Result<()> {
    let application = resolve_application(client, name_or_id).await?;
    let status = client
        .application_status(application.application.id().as_str())
        .await?;
    console.emit(&ShowReport {
        application: &application,
        status: &status,
    })?;
    if let Some(message) = &status.message {
        console.warning(message)?;
    }
    Ok(())
}

/// Emits a bounded snapshot of runtime logs, warning when the daemon truncated it.
async fn logs(
    console: &mut Console,
    client: &Client,
    name_or_id: &str,
    service: Option<&str>,
    tail: u16,
    since_seconds: u32,
) -> Result<()> {
    let app = resolve_application(client, name_or_id).await?;
    let logs = client
        .application_logs(app.application.id().as_str(), service, tail, since_seconds)
        .await?;
    console.emit(&logs)?;
    if logs.truncated {
        console.warning("Log snapshot was truncated; narrow the service or time window.")?;
    }
    Ok(())
}

/// Validates a manifest with the same rules as the daemon, without a daemon.
pub(crate) async fn validate(console: &mut Console, file: &Path) -> Result<()> {
    let manifest = read_manifest(file).await?;
    console.emit(&ValidManifestReport {
        application: manifest_name(&manifest, file)?,
    })
}

/// Previews a manifest. A blocked plan is still printed, then fails with a
/// conflict error that repeats the blocking diagnostics.
async fn plan_command(console: &mut Console, client: &Client, args: &ManifestArgs) -> Result<()> {
    let manifest = read_manifest(&args.file).await?;
    let plan = client
        .plan_application_toml_with_generation(&manifest, args.expected_generation)
        .await?;
    console.emit(&plan)?;
    if plan.plan.is_blocked() {
        return Err(CliError::blocked_plan(&plan));
    }
    Ok(())
}

/// Saves a TOML manifest, optionally deploying it:
/// 1. Reads the manifest and finds any existing application with the same name.
/// 2. Confirms, then saves with preconditions: the expected generation (defaulting to
///    the current one, or `0` for a new application) and the existing ID, unless `--force`.
/// 3. If a deployment was accepted and `--no-wait` is unset, waits for it to finish.
async fn apply(cli: &Cli, client: &Client, console: &mut Console, args: &ApplyArgs) -> Result<()> {
    let manifest = read_manifest(&args.file).await?;
    let name = manifest_name(&manifest, &args.file)?;
    // Configuration inspection does not depend on Docker availability.
    let current = find_by_name(client, &name).await?;
    let generation = current.as_ref().map_or(0, |app| app.generation);
    let id = current.as_ref().map(|app| app.id.as_str());
    let action = if args.deployment.deploy {
        "Save and deploy"
    } else {
        "Save"
    };
    confirm(
        console,
        cli.noninteractive,
        args.yes,
        &format!("{action} application {name:?}? [y/N] "),
    )
    .await?;
    let saved = retry_transport(|| {
        client.apply_application_toml_with_preconditions(
            &manifest,
            (!args.force).then_some(args.expected_generation.unwrap_or(generation)),
            if args.force { None } else { id },
            args.force,
            args.deployment.deploy,
        )
    })
    .await?;
    let Some(operation_id) = saved.operation_id.as_deref() else {
        return console.emit(&saved);
    };
    if args.deployment.no_wait {
        return console.emit(&saved);
    }
    let operation = wait_for_operation(console, client, operation_id).await?;
    console.emit(&SavedDeploymentReport {
        saved: &saved,
        outcome: operation.state,
        operation: &operation,
    })
}

/// Confirms and deletes an application (named volumes are retained), guarded by
/// the expected generation unless `--force`. Waits until the application is gone
/// unless `--no-wait`.
async fn delete(
    cli: &Cli,
    client: &Client,
    console: &mut Console,
    args: &DeleteArgs,
) -> Result<()> {
    let application = resolve_application(client, &args.name_or_id).await?;
    console.info(format_args!(
        "deleting {} ({}): managed services and network are removed; named volumes are retained",
        application.application.metadata().name,
        application.application.id()
    ))?;

    confirm(
        console,
        cli.noninteractive,
        args.yes,
        &format!(
            "Delete application {:?}? Named volumes will be retained. [y/N] ",
            application.application.metadata().name
        ),
    )
    .await?;

    let accepted = retry_transport(|| {
        client.delete_application_with_preconditions(
            application.application.id().as_str(),
            (!args.force).then_some(args.expected_generation.unwrap_or(application.generation)),
            args.force,
        )
    })
    .await
    .map_err(CliError::from)?;
    if args.no_wait {
        return console.emit(&DeletionReport::accepted(&accepted));
    }
    wait_for_deletion(
        console,
        client,
        &accepted.application_id,
        &accepted.operation_id,
    )
    .await?;
    console.emit(&DeletionReport::completed(&accepted))
}

/// Confirms and retries the latest operation with its saved inputs, guarded by the
/// expected generation only when one is given. Waits for it unless `--no-wait`.
async fn reconcile(
    cli: &Cli,
    client: &Client,
    console: &mut Console,
    args: &TargetArgs,
) -> Result<()> {
    let application = resolve_application(client, &args.name_or_id).await?;
    confirm(
        console,
        cli.noninteractive,
        args.yes,
        &format!(
            "Retry the latest operation for application {:?}? [y/N] ",
            application.application.metadata().name
        ),
    )
    .await?;
    let accepted = retry_transport(|| {
        client.reconcile_application(
            application.application.id().as_str(),
            args.expected_generation,
        )
    })
    .await?;
    wait_for_accepted(console, client, args.no_wait, &accepted).await
}

/// Confirms and deploys saved configuration, fetching the manifest and resolving
/// sources again (from `--branch` or `--commit` for this deployment only). Guarded by
/// the expected generation, defaulting to the current one. Waits unless `--no-wait`.
async fn deploy(
    cli: &Cli,
    client: &Client,
    console: &mut Console,
    args: &DeployArgs,
) -> Result<()> {
    let target = &args.target;
    let application = resolve_application(client, &target.name_or_id).await?;
    confirm(
        console,
        cli.noninteractive,
        target.yes,
        &format!(
            "Deploy application {:?}? [y/N] ",
            application.application.metadata().name
        ),
    )
    .await?;
    let revision = args.revision();
    let accepted = retry_transport(|| {
        client.deploy_application(
            application.application.id().as_str(),
            target.expected_generation.unwrap_or(application.generation),
            revision.as_ref(),
        )
    })
    .await?;
    wait_for_accepted(console, client, target.no_wait, &accepted).await
}

/// Emits an accepted operation as is with `no_wait`, otherwise waits for it and
/// emits its outcome.
async fn wait_for_accepted(
    console: &mut Console,
    client: &Client,
    no_wait: bool,
    accepted: &AcceptedOperation,
) -> Result<()> {
    if no_wait {
        return console.emit(accepted);
    }
    let operation = wait_for_operation(console, client, &accepted.operation_id).await?;
    console.emit(&OperationOutcomeReport {
        accepted,
        outcome: operation.state,
        operation: &operation,
    })
}

/// Shows one operation, polling until it reaches a terminal state unless `--no-wait`.
async fn operation(console: &mut Console, client: &Client, args: &OperationArgs) -> Result<()> {
    let operation = if args.no_wait {
        client.operation(&args.operation_id).await?
    } else {
        wait_for_operation(console, client, &args.operation_id).await?
    };
    console.emit(&operation)?;
    if let Some(message) = &operation.error_message {
        console.warning(message)?;
    }
    Ok(())
}

/// Collects every application summary across all pages.
async fn all_applications(client: &Client) -> Result<Vec<ApplicationSummary>> {
    fold_applications(client, Vec::new(), |applications, application| {
        applications.push(application);
    })
    .await
}

/// Walks every page of the application list, folding each summary into `value`.
/// Fails if the daemon repeats a cursor, which would otherwise loop forever.
async fn fold_applications<T>(
    client: &Client,
    mut value: T,
    mut fold: impl FnMut(&mut T, ApplicationSummary),
) -> Result<T> {
    let mut cursor = None;
    let mut seen_cursors = BTreeSet::new();
    loop {
        let page: Page<ApplicationSummary> = client
            .applications_with(&ListApplicationsOptions {
                cursor: cursor.clone(),
                limit: Some(PAGE_SIZE),
            })
            .await?;
        for application in page.items {
            fold(&mut value, application);
        }
        let Some(next_cursor) = page.next_cursor else {
            return Ok(value);
        };
        if !seen_cursors.insert(next_cursor.clone()) {
            return Err(CliError::new(
                ErrorKind::General,
                "the daemon returned a repeated pagination cursor",
            )
            .invalid_response());
        }
        cursor = Some(next_cursor);
    }
}

/// Finds the application with exactly this name. More than one match is a conflict.
async fn find_by_name(client: &Client, name: &str) -> Result<Option<ApplicationSummary>> {
    let matches = fold_applications(client, Vec::new(), |matches, application| {
        if application.name == name {
            matches.push(application);
        }
    })
    .await?;
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.into_iter().next()),
        count => Err(CliError::new(
            ErrorKind::Conflict,
            format!("application name {name:?} matched {count} applications"),
        )),
    }
}

/// Loads an application from a name or ID. Values shaped like an ID are tried as an
/// ID first and fall back to a name lookup on 404, since a name can look like an ID.
pub(crate) async fn resolve_application(
    client: &Client,
    name_or_id: &str,
) -> Result<ApplicationView> {
    if looks_like_application_id(name_or_id) {
        match client.application(name_or_id).await {
            Ok(application) => return Ok(application),
            // The value may still be an application name, so fall through to
            // the name lookup.
            Err(ClientError::Api { status, .. }) if status.as_u16() == 404 => {}
            Err(error) => return Err(error.into()),
        }
    }
    let summary = find_by_name(client, name_or_id).await?.ok_or_else(|| {
        CliError::new(
            ErrorKind::Input,
            format!("application {name_or_id:?} was not found"),
        )
    })?;
    Ok(client.application(summary.id.as_str()).await?)
}

/// Polls an operation every `POLL_INTERVAL` with a progress task until it is
/// terminal. Returns an `ErrorKind::Operation` error unless it succeeded or was superseded.
pub(crate) async fn wait_for_operation(
    console: &mut Console,
    client: &Client,
    operation_id: &str,
) -> Result<Operation> {
    let progress = console.start_task(operation_id);
    loop {
        let operation = client.operation(operation_id).await?;
        if operation.state.terminal() {
            progress.finish(
                OperationProgress::outcome(&operation),
                &OperationProgress::message(&operation),
            );
            return finish_operation(operation);
        }
        progress.update(&OperationProgress::message(&operation));
        time::sleep(POLL_INTERVAL).await;
    }
}

/// Accepts `Succeeded` and `Superseded` terminal states; any other state becomes an
/// operation error whose details carry the full operation for the error report.
fn finish_operation(operation: Operation) -> Result<Operation> {
    if matches!(
        operation.state,
        OperationState::Succeeded | OperationState::Superseded
    ) {
        Ok(operation)
    } else {
        let message = format!("operation ended in state {}", operation.state);
        Err(CliError::new(ErrorKind::Operation, message)
            .with_details(json!({"operation": operation})))
    }
}

/// Polls until the application returns 404, which is the success condition.
/// Meanwhile tracks the delete operation for progress and fails early if it ends
/// unsuccessfully. A 404 for the operation itself is tolerated.
async fn wait_for_deletion(
    console: &mut Console,
    client: &Client,
    id: &str,
    operation_id: &str,
) -> Result<()> {
    let progress = console.start_task(operation_id);
    loop {
        match client.application(id).await {
            Err(ClientError::Api { status, .. }) if status.as_u16() == 404 => {
                progress.finish(TaskOutcome::Succeeded, "deleted");
                return Ok(());
            }
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        match client.operation(operation_id).await {
            Ok(operation) => {
                progress.update(&OperationProgress::message(&operation));
                if operation.state.terminal() {
                    if !matches!(
                        operation.state,
                        OperationState::Succeeded | OperationState::Superseded
                    ) {
                        progress
                            .finish(TaskOutcome::Failed, &OperationProgress::message(&operation));
                    }
                    finish_operation(operation)?;
                }
            }
            Err(ClientError::Api { status, .. }) if status.as_u16() == 404 => {}
            Err(error) => return Err(error.into()),
        }
        time::sleep(POLL_INTERVAL).await;
    }
}

/// Maps operations onto progress-task messages and outcomes.
struct OperationProgress;
impl OperationProgress {
    /// One-line progress text.
    ///
    /// ```text
    /// running, phase "pulling_images", resource "web"  →  running · Pulling images · web
    /// ```
    fn message(operation: &Operation) -> String {
        let mut phase = operation.phase.as_deref().unwrap_or("").replace('_', " ");
        if let Some(first) = phase.get_mut(..1) {
            first.make_ascii_uppercase();
        }
        format!(
            "{} · {phase}{}",
            operation.state,
            operation
                .resource
                .as_ref()
                .map_or_else(String::new, |r| format!(" · {r}"))
        )
    }
    /// Superseded operations are shown as skipped rather than failed.
    fn outcome(operation: &Operation) -> TaskOutcome {
        match operation.state {
            OperationState::Succeeded => TaskOutcome::Succeeded,
            OperationState::Superseded => TaskOutcome::Skipped,
            _ => TaskOutcome::Failed,
        }
    }
}
