use crate::{
    cli::{ApplyArgs, Cli, Command, DeleteArgs, ManifestArgs, OperationArgs},
    error::{CliError, ErrorKind, Result},
    output::{
        blocked_plan_error, emit_json, render_operation, render_plan, render_plan_stderr,
        report_operation,
    },
    support::{
        confirm, desired_replicas, idempotency_key, looks_like_application_id, manifest_name,
        read_manifest, retry_transport, terminal_operation,
    },
};
use futures_util::StreamExt;
use piqueld_client::{
    ApplicationView, Client, ClientError, ListApplicationsOptions, OperationView, Page, PlanView,
    Source,
};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fmt::Write as _, io, path::PathBuf};
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
    // Leave room inside the complete command deadline for a second transport
    // attempt when an idempotent mutation fails promptly.
    Ok(client.with_timeout(cli.timeout / 2))
}

async fn status(cli: &Cli, client: &Client) -> Result<()> {
    let status = client.system_status().await?;
    if cli.json {
        return emit_json(&status);
    }
    println!(
        "daemon {} (version {}, API {}, instance {})",
        status.status, status.daemon_version, status.api_version, status.instance_id
    );
    println!("transport: {}", transport_description(cli));
    Ok(())
}

async fn list(cli: &Cli, client: &Client) -> Result<()> {
    let applications = all_applications(client).await?;
    let statuses = futures_util::stream::iter(applications.iter().map(|application| async {
        client
            .application_status(application.application.id.as_str())
            .await
    }))
    .buffered(8)
    .collect::<Vec<_>>()
    .await;
    let mut rows = Vec::with_capacity(applications.len());
    for (application, status) in applications.into_iter().zip(statuses) {
        let status = match status {
            Ok(status) => Some(status),
            Err(error) if cli.json => return Err(error.into()),
            Err(error) => {
                eprintln!(
                    "  {}: status unavailable: {}",
                    application.application.metadata.name, error
                );
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
        println!("No applications.");
    } else {
        for (application, status) in rows {
            let state = status
                .as_ref()
                .map_or_else(|| "unavailable".to_owned(), |status| status.state.clone());
            println!(
                "{}\t{}\tgeneration {}\tdesired replicas {}\t{}",
                application.application.metadata.name,
                application.application.id,
                application.generation,
                desired_replicas(&application),
                state,
            );
            if let Some(status) = status
                && let Some(message) = &status.message
            {
                eprintln!("  {}: {message}", application.application.metadata.name);
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
    println!(
        "{} ({})",
        application.application.metadata.name, application.application.id
    );
    println!(
        "generation {} (observed {})\tstate {}",
        application.generation,
        status
            .observed_generation
            .map_or_else(|| "none".to_owned(), |generation| generation.to_string()),
        status.state,
    );
    println!("desired replicas: {}", desired_replicas(&application));
    for service in &application.application.spec.services {
        let image = match &service.source {
            Source::Image { image } => image,
        };
        println!(
            "service {}: {} replica(s), image {image}",
            service.name, service.replicas
        );
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
        println!("named volumes: {volumes} (retained on deletion)");
    }
    if let Some(message) = status.message {
        eprintln!("diagnostic: {message}");
    }
    Ok(())
}

async fn plan_command(cli: &Cli, client: &Client, args: &ManifestArgs) -> Result<()> {
    let manifest = read_manifest(&args.file).await?;
    let name = manifest_name(&manifest, &args.file)?;
    let (plan, _) = prepare_plan(client, &manifest, &name, args.expected_generation).await?;
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
    let (plan, existing) = prepare_plan(client, &manifest, &name, args.expected_generation).await?;
    render_plan_stderr(&plan).map_err(|error| {
        CliError::new(ErrorKind::General, format!("could not write plan: {error}"))
    })?;
    if plan.plan.is_blocked() {
        return Err(blocked_plan_error(&plan, true));
    }
    confirm(args.yes, &format!("Apply application {name:?}? [y/N] ")).await?;

    let key = idempotency_key();
    let accepted = if let Some(application) = existing {
        let expected = args.expected_generation.unwrap_or(application.generation);
        retry_transport(|| {
            client.replace_application_toml_with_key(
                application.application.id.as_str(),
                &manifest,
                expected,
                Some(&key),
            )
        })
        .await
        .map_err(CliError::from)?
    } else {
        retry_transport(|| client.create_application_toml(&manifest, &key))
            .await
            .map_err(CliError::from)?
    };
    if args.no_wait {
        if cli.json {
            return emit_json(&accepted);
        }
        println!(
            "accepted operation {} for application {}",
            accepted.operation_id, accepted.application_id
        );
        return Ok(());
    }
    let operation = wait_for_operation(client, &accepted.operation_id, None).await?;
    if cli.json {
        return emit_json(&json!({"accepted": accepted, "operation": operation}));
    }
    println!("operation {} {}", operation.id, operation.state);
    Ok(())
}

async fn delete(cli: &Cli, client: &Client, args: &DeleteArgs) -> Result<()> {
    let application = resolve_application(client, &args.name_or_id).await?;
    let expected = args.expected_generation.unwrap_or(application.generation);
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

    let key = idempotency_key();
    let request = piqueld_client::DeleteApplicationRequest {
        expected_generation: expected,
    };
    let accepted = retry_transport(|| {
        client.delete_application_with_key(
            application.application.id.as_str(),
            &request,
            Some(&key),
        )
    })
    .await
    .map_err(CliError::from)?;
    if args.no_wait {
        if cli.json {
            return emit_json(&json!({"accepted": accepted, "volumes_retained": true}));
        }
        println!(
            "accepted operation {} (named volumes retained)",
            accepted.operation_id
        );
        return Ok(());
    }
    let operation = wait_for_operation(client, &accepted.operation_id, None).await?;
    if cli.json {
        return emit_json(&json!({
            "accepted": accepted,
            "operation": operation,
            "volumes_retained": true,
        }));
    }
    println!(
        "operation {} {} (named volumes retained)",
        operation.id, operation.state
    );
    Ok(())
}

async fn operation(cli: &Cli, client: &Client, args: &OperationArgs) -> Result<()> {
    let initial = client.operation(&args.operation_id).await?;
    if args.no_wait {
        render_operation(cli, &initial)?;
        return Ok(());
    }
    let operation = wait_for_operation(client, &args.operation_id, Some(initial)).await?;
    render_operation(cli, &operation)
}

async fn prepare_plan(
    client: &Client,
    manifest: &str,
    name: &str,
    expected_generation: Option<u64>,
) -> Result<(PlanView, Option<ApplicationView>)> {
    let existing = find_by_name(client, name).await?;
    let plan = if let Some(application) = &existing {
        let expected = expected_generation.unwrap_or(application.generation);
        client
            .plan_replace_toml(application.application.id.as_str(), manifest, expected)
            .await?
    } else {
        if expected_generation.is_some() {
            return Err(CliError::new(
                ErrorKind::Conflict,
                format!(
                    "expected generation was supplied, but application {name:?} does not exist"
                ),
            ));
        }
        client.plan_create_toml(manifest).await?
    };
    Ok((plan, existing))
}

async fn all_applications(client: &Client) -> Result<Vec<ApplicationView>> {
    fold_applications(client, Vec::new(), |applications, application| {
        applications.push(application);
    })
    .await
}

async fn fold_applications<T>(
    client: &Client,
    mut value: T,
    mut fold: impl FnMut(&mut T, ApplicationView),
) -> Result<T> {
    let mut cursor = None;
    let mut seen_cursors = BTreeSet::new();
    loop {
        let page: Page<ApplicationView> = client
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

async fn find_by_name(client: &Client, name: &str) -> Result<Option<ApplicationView>> {
    let matches = fold_applications(client, Vec::new(), |matches, application| {
        if application.application.metadata.name == name {
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
    find_by_name(client, name_or_id).await?.ok_or_else(|| {
        CliError::new(
            ErrorKind::Input,
            format!("application {name_or_id:?} was not found"),
        )
    })
}

async fn wait_for_operation(
    client: &Client,
    operation_id: &str,
    initial: Option<OperationView>,
) -> Result<OperationView> {
    let mut current = initial;
    loop {
        let operation = match current.take() {
            Some(operation) => operation,
            None => client.operation(operation_id).await?,
        };
        report_operation(&operation);
        if terminal_operation(&operation.state) {
            return finish_operation(operation);
        }
        tokio::select! {
            result = signal::ctrl_c() => {
                result.map_err(|_| CliError::new(ErrorKind::General, "could not install Ctrl-C handler"))?;
                return Err(CliError::new(
                    ErrorKind::Interrupted,
                    "wait interrupted; the server-side operation was not cancelled",
                ));
            }
            () = time::sleep(POLL_INTERVAL) => {}
        }
    }
}

fn finish_operation(operation: OperationView) -> Result<OperationView> {
    if matches!(
        operation.state.to_ascii_lowercase().as_str(),
        "succeeded" | "completed"
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
        Err(CliError::new(ErrorKind::Operation, message)
            .with_details(json!({"operation": operation})))
    }
}
