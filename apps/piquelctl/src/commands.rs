use crate::{
    cli::{
        ApplyArgs, Cli, Command, DeleteArgs, ManifestArgs, OperationArgs, ReconcileArgs, RenameArgs,
    },
    error::{CliError, ErrorKind, Result},
    output::{
        blocked_plan_error, emit_json, render_operation, render_plan, render_plan_stderr,
        report_operation,
    },
    support::{
        confirm, desired_replicas, looks_like_application_id, manifest_name, read_manifest,
        retry_transport,
    },
};
use futures_util::StreamExt;
use piqueld_client::{
    ApplicationSummary, ApplicationView, Client, ClientError, ListApplicationsOptions, Operation,
    OperationState, Page, Source,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fmt::Write as _,
    io::{self, Write as _},
    path::PathBuf,
};
use tokio::{signal, time};

use crate::support::{DEFAULT_SOCKET, PAGE_SIZE, POLL_INTERVAL, transport_description};

pub(crate) async fn run(cli: &Cli) -> Result<()> {
    let client = build_client(cli)?;
    match &cli.command {
        Command::Status => status(cli, &client).await,
        Command::List => list(cli, &client).await,
        Command::Show { name_or_id } => show(cli, &client, name_or_id).await,
        Command::Plan(args) => plan_command(cli, &client, args).await,
        Command::Apply(args) => apply(cli, &client, args).await,
        Command::Delete(args) => delete(cli, &client, args).await,
        Command::Operation(args) => operation(cli, &client, args).await,
        Command::Reconcile(args) => reconcile_or_refresh(cli, &client, args, false).await,
        Command::Rename(args) => rename(cli, &client, args).await,
        Command::Refresh(args) => reconcile_or_refresh(cli, &client, args, true).await,
        Command::Events {
            application,
            cursor,
            limit,
        } => {
            let page = client
                .events(application.as_deref(), cursor.as_deref(), *limit)
                .await?;
            if cli.json {
                return emit_json(&page);
            }
            for event in page.items {
                writeln!(
                    io::stdout().lock(),
                    "{}\t{}\t{}\t{}\tattempt {}\t{}",
                    event.id,
                    event.created_at_ms,
                    event.kind,
                    event.operation_id.as_deref().unwrap_or("-"),
                    event
                        .attempt
                        .map_or_else(|| "-".into(), |attempt| attempt.to_string()),
                    format_args!(
                        "{} {} {} {}",
                        event.phase.as_deref().unwrap_or(""),
                        event.resource.as_deref().unwrap_or(""),
                        event.error_code.as_deref().unwrap_or(""),
                        event.message.as_deref().unwrap_or("")
                    )
                )?;
            }
            if let Some(cursor) = page.next_cursor {
                writeln!(io::stdout().lock(), "next cursor: {cursor}")?;
            }
            Ok(())
        }
    }
}

fn build_client(cli: &Cli) -> Result<Client> {
    let client = if let Some(url) = &cli.url {
        Client::tcp(url).map_err(CliError::from)?
    } else {
        Client::unix(
            cli.socket
                .clone()
                .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET)),
        )
    };
    Ok(client
        .with_timeout(cli.timeout)
        .with_request_id(uuid::Uuid::now_v7().to_string()))
}

async fn status(cli: &Cli, client: &Client) -> Result<()> {
    let status = client.system_status().await?;
    if cli.json {
        return emit_json(&status);
    }
    writeln!(
        io::stdout().lock(),
        "daemon {} (version {}, API {}, instance {})",
        status.status,
        status.daemon_version,
        status.api_version,
        status.instance_id
    )?;
    writeln!(
        io::stdout().lock(),
        "transport: {}",
        transport_description(cli)
    )?;
    Ok(())
}

async fn list(cli: &Cli, client: &Client) -> Result<()> {
    let applications = all_applications(client).await?;
    let statuses = futures_util::stream::iter(
        applications
            .iter()
            .map(|application| async { client.application_status(application.id.as_str()).await }),
    )
    .buffered(8)
    .collect::<Vec<_>>()
    .await;
    let mut rows = Vec::with_capacity(applications.len());
    for (application, status) in applications.into_iter().zip(statuses) {
        let status = match status {
            Ok(status) => Some(status),
            Err(error) if cli.json => return Err(error.into()),
            Err(error) => {
                eprintln!("  {}: status unavailable: {}", application.name, error);
                None
            }
        };
        rows.push((application, status));
    }
    if cli.json {
        let items = rows
            .iter()
            .map(|(application, status)| {
                json!({
                    "application": application,
                    "status": status,
                })
            })
            .collect::<Vec<_>>();
        return emit_json(&json!({"items": items, "next_cursor": Value::Null}));
    }
    if rows.is_empty() {
        writeln!(io::stdout().lock(), "No applications.")?;
    } else {
        for (application, status) in rows {
            let state = status.as_ref().map_or_else(
                || "unavailable".to_owned(),
                |status| status.state.to_string(),
            );
            writeln!(
                io::stdout().lock(),
                "{}\t{}\tgeneration {}\t{}",
                application.name,
                application.id,
                application.generation,
                state,
            )?;
            if let Some(status) = status
                && let Some(message) = &status.message
            {
                eprintln!("  {}: {message}", application.name);
            }
        }
    }
    Ok(())
}

async fn show(cli: &Cli, client: &Client, name_or_id: &str) -> Result<()> {
    let application = resolve_application(client, name_or_id).await?;
    let status = client
        .application_status(application.application.id.as_str())
        .await?;
    if cli.json {
        return emit_json(&json!({"application": application, "status": status}));
    }
    writeln!(
        io::stdout().lock(),
        "{} ({})",
        application.application.metadata.name,
        application.application.id
    )?;
    writeln!(
        io::stdout().lock(),
        "intent state {}, observed runtime {}",
        status.state,
        status.runtime_health.as_deref().unwrap_or("unknown")
    )?;
    writeln!(
        io::stdout().lock(),
        "intent generation {}, resolved target generation {}",
        application.generation,
        application
            .resolved_generation
            .map_or_else(|| "none".to_owned(), |value| value.to_string())
    )?;
    writeln!(
        io::stdout().lock(),
        "desired replicas: {}",
        desired_replicas(&application)
    )?;
    for service in &application.application.spec.services {
        let image = match &service.source {
            Source::Image { image } => image,
        };
        writeln!(
            io::stdout().lock(),
            "service {}: {} replica(s), image {image}",
            service.name,
            service.replicas
        )?;
    }
    if !application.application.spec.volumes.is_empty() {
        let volumes = application
            .application
            .spec
            .volumes
            .iter()
            .map(|volume| volume.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(
            io::stdout().lock(),
            "named volumes: {volumes} (retained on deletion)"
        )?;
    }
    if let Some(message) = status.message {
        eprintln!("diagnostic: {message}");
    }
    Ok(())
}

async fn plan_command(cli: &Cli, client: &Client, args: &ManifestArgs) -> Result<()> {
    let manifest = read_manifest(&args.file).await?;
    let plan = client
        .plan_application_toml_with_generation(&manifest, args.expected_generation)
        .await?;
    if cli.json {
        emit_json(&plan)?;
        if plan.plan.is_blocked() {
            return Err(blocked_plan_error(&plan, false));
        }
        return Ok(());
    }
    render_plan(&plan, &mut io::stdout()).map_err(|error| {
        CliError::new(ErrorKind::General, format!("could not write plan: {error}"))
    })?;
    if plan.plan.is_blocked() {
        return Err(blocked_plan_error(&plan, true));
    }
    Ok(())
}

async fn apply(cli: &Cli, client: &Client, args: &ApplyArgs) -> Result<()> {
    let manifest = read_manifest(&args.file).await?;
    let name = manifest_name(&manifest, &args.file)?;
    let plan = client
        .plan_application_toml_with_generation(&manifest, args.expected_generation)
        .await?;
    if plan.identical && !args.force {
        let operation = plan.operation.ok_or_else(|| {
            CliError::new(ErrorKind::General, "identical application has no operation")
        })?;
        let operation = if args.no_wait {
            if operation.state.terminal() {
                finish_operation(operation)?
            } else {
                operation
            }
        } else {
            wait_for_operation(client, &operation.id.clone(), Some(operation)).await?
        };
        if cli.json {
            return emit_json(&json!({
                "identical": true, "application_id": plan.application_id,
                "outcome": operation.state, "operation": operation,
            }));
        }
        writeln!(
            io::stdout().lock(),
            "This is identical to the existing manifest."
        )?;
        return render_operation(cli, &operation);
    }
    render_plan_stderr(&plan).map_err(|error| {
        CliError::new(ErrorKind::General, format!("could not write plan: {error}"))
    })?;
    if plan.plan.is_blocked() {
        return Err(blocked_plan_error(&plan, true));
    }
    confirm(args.yes, &format!("Apply application {name:?}? [y/N] ")).await?;

    let accepted = retry_transport(|| {
        client.apply_application_toml_with_preconditions(
            &manifest,
            (!args.force).then_some(args.expected_generation.unwrap_or(plan.generation)),
            (!args.force && plan.generation > 0).then_some(plan.application_id.as_str()),
            args.force,
        )
    })
    .await?;
    if args.no_wait {
        if cli.json {
            return emit_json(&accepted);
        }
        writeln!(
            io::stdout().lock(),
            "accepted operation {} for application {}",
            accepted.operation_id,
            accepted.application_id
        )?;
        return Ok(());
    }
    let operation = wait_for_operation(client, &accepted.operation_id, None).await?;
    if cli.json {
        return emit_json(
            &json!({"accepted": accepted, "outcome": operation.state, "operation": operation}),
        );
    }
    writeln!(
        io::stdout().lock(),
        "operation {} {}",
        operation.id,
        operation.state
    )?;
    Ok(())
}

async fn delete(cli: &Cli, client: &Client, args: &DeleteArgs) -> Result<()> {
    let application = resolve_application(client, &args.name_or_id).await?;
    eprintln!(
        "deleting {} ({}): managed services and network are removed; named volumes are retained",
        application.application.metadata.name, application.application.id
    );
    confirm(
        args.yes,
        &format!(
            "Delete application {:?}? Named volumes will be retained. [y/N] ",
            application.application.metadata.name
        ),
    )
    .await?;

    let accepted = retry_transport(|| {
        client.delete_application_with_preconditions(
            application.application.id.as_str(),
            (!args.force).then_some(args.expected_generation.unwrap_or(application.generation)),
            args.force,
        )
    })
    .await
    .map_err(CliError::from)?;
    if args.no_wait {
        if cli.json {
            return emit_json(&json!({"accepted": accepted, "volumes_retained": true}));
        }
        writeln!(
            io::stdout().lock(),
            "accepted operation {} (named volumes retained)",
            accepted.operation_id
        )?;
        return Ok(());
    }
    let operation = wait_for_operation(client, &accepted.operation_id, None).await?;
    if cli.json {
        return emit_json(&json!({
            "accepted": accepted,
            "outcome": operation.state,
            "operation": operation,
            "volumes_retained": true,
        }));
    }
    writeln!(
        io::stdout().lock(),
        "operation {} {} (named volumes retained)",
        operation.id,
        operation.state
    )?;
    Ok(())
}

async fn operation(cli: &Cli, client: &Client, args: &OperationArgs) -> Result<()> {
    if args.no_wait {
        let initial = client.operation(&args.operation_id).await?;
        render_operation(cli, &initial)?;
        return Ok(());
    }
    let operation = wait_for_operation(client, &args.operation_id, None).await?;
    render_operation(cli, &operation)
}

async fn all_applications(client: &Client) -> Result<Vec<ApplicationSummary>> {
    fold_applications(client, Vec::new(), |applications, application| {
        applications.push(application);
    })
    .await
}

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
            ));
        }
        cursor = Some(next_cursor);
    }
}

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

async fn resolve_application(client: &Client, name_or_id: &str) -> Result<ApplicationView> {
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

async fn wait_for_operation(
    client: &Client,
    operation_id: &str,
    initial: Option<Operation>,
) -> Result<Operation> {
    let wait = async {
        let mut current = initial;
        loop {
            let operation = match current.take() {
                Some(operation) => operation,
                None => client.operation(operation_id).await?,
            };
            report_operation(&operation);
            if operation.state.terminal() {
                return finish_operation(operation);
            }
            time::sleep(POLL_INTERVAL).await;
        }
    };
    tokio::select! {
        result = wait => result,
        result = signal::ctrl_c() => {
            result.map_err(|error| CliError::new(
                ErrorKind::General,
                format!("could not install Ctrl-C handler: {error}"),
            ))?;
            Err(CliError::new(
                ErrorKind::Interrupted,
                "wait interrupted; the server-side operation was not cancelled",
            ))
        }
    }
}

fn finish_operation(operation: Operation) -> Result<Operation> {
    if matches!(
        operation.state,
        OperationState::Succeeded | OperationState::Superseded
    ) {
        Ok(operation)
    } else {
        let mut message = format!(
            "operation {} ended in state {}",
            operation.id, operation.state
        );
        if let Some(code) = &operation.error_code {
            let _ = write!(message, " ({code})");
        }
        if let Some(error) = &operation.error_message {
            message.push_str(": ");
            message.push_str(error);
        }
        message.push_str("; use piquelctl reconcile to retry current intent");
        Err(CliError::new(ErrorKind::Operation, message)
            .with_details(json!({"operation": operation})))
    }
}

async fn reconcile_or_refresh(
    cli: &Cli,
    client: &Client,
    args: &ReconcileArgs,
    refresh: bool,
) -> Result<()> {
    let application = resolve_application(client, &args.name_or_id).await?;
    let action = if refresh {
        "Refresh images for"
    } else {
        "Reconcile current intent for"
    };
    confirm(
        args.yes,
        &format!(
            "{action} application {:?}? [y/N] ",
            application.application.metadata.name
        ),
    )
    .await?;
    let id = application.application.id.as_str();
    let accepted = retry_transport(|| async {
        if refresh {
            client
                .refresh_application(id, args.expected_generation)
                .await
        } else {
            client
                .reconcile_application(id, args.expected_generation)
                .await
        }
    })
    .await?;
    if args.no_wait {
        if cli.json {
            return emit_json(&accepted);
        }
        writeln!(
            io::stdout().lock(),
            "accepted operation {} for application {}",
            accepted.operation_id,
            accepted.application_id
        )?;
        return Ok(());
    }
    let operation = wait_for_operation(client, &accepted.operation_id, None).await?;
    if cli.json {
        emit_json(&json!({"accepted":accepted,"outcome":operation.state,"operation":operation}))
    } else {
        render_operation(cli, &operation)
    }
}

async fn rename(cli: &Cli, client: &Client, args: &RenameArgs) -> Result<()> {
    let application = resolve_application(client, &args.name_or_id).await?;
    confirm(
        args.yes,
        &format!(
            "Rename application {:?} to {:?}? [y/N] ",
            application.application.metadata.name, args.new_name
        ),
    )
    .await?;
    let request = piqueld_client::RenameApplicationRequest {
        name: args.new_name.clone(),
        expected_generation: (!args.force)
            .then_some(args.expected_generation.unwrap_or(application.generation)),
    };
    let renamed = retry_transport(|| {
        client.rename_application_with_force(
            application.application.id.as_str(),
            &request,
            args.force,
        )
    })
    .await?;
    if cli.json {
        emit_json(&renamed)?;
    } else {
        writeln!(
            io::stdout().lock(),
            "Renamed {} to {} (generation {}).",
            application.application.metadata.name,
            renamed.name,
            renamed.generation
        )?;
    }
    eprintln!(
        "Update metadata.name to {:?} in your manifest file before applying it again.",
        renamed.name
    );
    Ok(())
}
