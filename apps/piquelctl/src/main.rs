//! Safe, small operator command-line client for the Plan 06 piqueld API.

use clap::{Args, Parser, Subcommand};
use piqueld_client::{
    ApplicationLogsOptions, ApplicationView, BuildView, Client, ClientError, ContainerLogView,
    ListApplicationsOptions, ListSecretsOptions, OperationView, Page, PlanView, SecretMetadata,
    Source, ValidationErrors,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fmt,
    fmt::Write as _,
    future::Future,
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};
use tokio::{fs, signal, time};
use uuid::Uuid;

const DEFAULT_SOCKET: &str = "/run/piqueld/piqueld.sock";
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const MAX_PAGINATION_PAGES: usize = 10_000;
const PAGE_SIZE: u16 = 100;
const MAX_SECRET_BYTES: usize = 500 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Essential commands for inspecting and operating Plan 06 applications.
#[derive(Debug, Parser)]
#[command(
    name = "piquelctl",
    version,
    about = "Operate a local piqueld control plane"
)]
struct Cli {
    /// Unix socket path. The default is the daemon's local socket.
    #[arg(long, global = true, value_name = "PATH", conflicts_with = "url")]
    socket: Option<PathBuf>,

    /// Explicit loopback HTTP endpoint, for example <http://127.0.0.1:8080/>.
    #[arg(long, global = true, value_name = "URL", conflicts_with = "socket")]
    url: Option<String>,

    /// Bound for each request and for the complete command wait.
    #[arg(long, global = true, default_value = "30s", value_parser = parse_duration)]
    timeout: Duration,

    /// Emit only the command's documented JSON result on stdout.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Report daemon availability and version.
    Status,
    /// List applications and their concise reconciliation status.
    List,
    /// Show one application by name or ID.
    Show {
        /// Application name or stable ID.
        name_or_id: String,
    },
    /// Preview creation or replacement from a TOML manifest.
    Plan(ManifestArgs),
    /// Plan, confirm, and apply a TOML manifest.
    Apply(ApplyArgs),
    /// Confirm and delete an application by name or ID.
    Delete(DeleteArgs),
    /// Inspect or wait for one asynchronous operation.
    Operation(OperationArgs),
    /// Manage logical secret metadata and values without exposing plaintext.
    Secret {
        #[command(subcommand)]
        command: SecretCommand,
    },
    /// Inspect durable source-build status associated with an application or operation.
    Build {
        #[command(subcommand)]
        command: BuildCommand,
    },
    /// Read one bounded historical log snapshot for an application.
    Logs(LogsArgs),
}

#[derive(Debug, Subcommand)]
enum BuildCommand {
    /// Inspect one build; its metadata includes application and operation IDs.
    Show {
        /// Stable build identifier.
        build_id: String,
    },
    /// List builds attached to one operation.
    Operation {
        /// Stable operation identifier.
        operation_id: String,
    },
}

#[derive(Debug, Args)]
struct LogsArgs {
    /// Application name or stable ID.
    name_or_id: String,

    /// Include records newer than this many seconds.
    #[arg(long)]
    since_seconds: Option<u64>,

    /// Maximum records returned.
    #[arg(long, default_value_t = 200, value_parser = clap::value_parser!(u16).range(1..=1000))]
    tail: u16,

    /// Maximum approximate response size.
    #[arg(long, default_value_t = 262_144, value_parser = clap::value_parser!(u32).range(1..=1_048_576))]
    max_bytes: u32,
}

#[derive(Debug, Subcommand)]
enum SecretCommand {
    /// List logical secret metadata; plaintext is never returned.
    List,
    /// Create or replace a logical secret from stdin or a protected file.
    Set(SecretSetArgs),
    /// Delete an unreferenced logical secret after confirmation.
    Delete(SecretDeleteArgs),
}

#[derive(Debug, Args)]
struct SecretSetArgs {
    /// Logical secret name.
    name: String,

    /// Read the value from a private, regular, symlink-free file.
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with = "stdin",
        required_unless_present = "stdin"
    )]
    file: Option<PathBuf>,

    /// Read the exact value bytes from a noninteractive stdin pipe.
    #[arg(long, conflicts_with = "file", required_unless_present = "file")]
    stdin: bool,
}

#[derive(Debug, Args)]
struct SecretDeleteArgs {
    /// Logical secret name.
    name: String,

    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct ManifestArgs {
    /// TOML application manifest.
    #[arg(long, value_name = "PATH")]
    file: PathBuf,

    /// Generation to require when replacing an existing application.
    #[arg(long, value_parser = parse_generation)]
    expected_generation: Option<u64>,
}

#[derive(Debug, Args)]
struct ApplyArgs {
    /// TOML application manifest.
    #[arg(long, value_name = "PATH")]
    file: PathBuf,

    /// Generation to require when replacing an existing application.
    #[arg(long, value_parser = parse_generation)]
    expected_generation: Option<u64>,

    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    yes: bool,

    /// Return after the daemon accepts the operation.
    #[arg(long)]
    no_wait: bool,
}

#[derive(Debug, Args)]
struct DeleteArgs {
    /// Application name or stable ID.
    name_or_id: String,

    /// Generation to require for deletion.
    #[arg(long, value_parser = parse_generation)]
    expected_generation: Option<u64>,

    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    yes: bool,

    /// Return after the daemon accepts the operation.
    #[arg(long)]
    no_wait: bool,
}

#[derive(Debug, Args)]
struct OperationArgs {
    /// Stable operation ID.
    operation_id: String,

    /// Fetch once instead of waiting for a terminal state.
    #[arg(long)]
    no_wait: bool,
}

#[derive(Clone, Copy, Debug)]
enum ErrorKind {
    General,
    Input,
    Conflict,
    Unavailable,
    Operation,
    Interrupted,
}

impl ErrorKind {
    const fn exit_code(self) -> u8 {
        match self {
            Self::General => 1,
            Self::Input => 2,
            Self::Conflict => 3,
            Self::Unavailable => 4,
            Self::Operation => 5,
            Self::Interrupted => 130,
        }
    }
}

#[derive(Debug)]
struct CliError {
    kind: ErrorKind,
    message: String,
    api_code: Option<String>,
    request_id: Option<String>,
    details: Option<Value>,
}

impl CliError {
    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            api_code: None,
            request_id: None,
            details: None,
        }
    }

    fn api(mut self, code: String, request_id: String, details: Value) -> Self {
        self.api_code = Some(code);
        self.request_id = (!request_id.is_empty()).then_some(request_id);
        self.details = (!details.is_null()).then_some(details);
        self
    }

    fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

impl From<ClientError> for CliError {
    fn from(error: ClientError) -> Self {
        match error {
            ClientError::Endpoint => Self::new(
                ErrorKind::Input,
                "HTTP endpoint must be a loopback http:// origin without credentials, path, query, or fragment",
            ),
            ClientError::Transport { message } => Self::new(
                ErrorKind::Unavailable,
                format!("could not connect to the piqueld API: {message}"),
            ),
            ClientError::Decode => Self::new(
                ErrorKind::General,
                "the daemon returned an invalid public API response",
            ),
            ClientError::SecretFile => Self::new(
                ErrorKind::Input,
                "secret input file must be a private, regular, symlink-free file",
            ),
            ClientError::Api { status, error } => {
                let kind = match status.as_u16() {
                    400 | 404 | 413 | 415 | 422 => ErrorKind::Input,
                    409 | 412 => ErrorKind::Conflict,
                    502..=504 => ErrorKind::Unavailable,
                    _ => ErrorKind::General,
                };
                Self::new(kind, format!("{} ({})", error.message, error.code)).api(
                    error.code,
                    error.request_id,
                    error.details,
                )
            }
        }
    }
}

type Result<T> = std::result::Result<T, CliError>;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let timeout = cli.timeout;
    let result = time::timeout(timeout, run(&cli)).await;
    match result {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(error)) => finish_error(&cli, error),
        Err(_) => finish_error(
            &cli,
            CliError::new(
                ErrorKind::Unavailable,
                format!("command timed out after {}", format_duration(timeout)),
            ),
        ),
    }
}

async fn run(cli: &Cli) -> Result<()> {
    let client = build_client(cli)?;
    match &cli.command {
        Command::Status => status(cli, &client).await,
        Command::List => list(cli, &client).await,
        Command::Show { name_or_id } => show(cli, &client, name_or_id).await,
        Command::Plan(args) => plan_command(cli, &client, args).await,
        Command::Apply(args) => apply(cli, &client, args).await,
        Command::Delete(args) => delete(cli, &client, args).await,
        Command::Operation(args) => operation(cli, &client, args).await,
        Command::Secret { command } => secret(cli, &client, command).await,
        Command::Build { command } => build(cli, &client, command).await,
        Command::Logs(args) => logs(cli, &client, args).await,
    }
}

fn build_client(cli: &Cli) -> Result<Client> {
    if cli.socket.is_some() && cli.url.is_some() {
        return Err(CliError::new(
            ErrorKind::Input,
            "--socket and --url cannot be used together",
        ));
    }
    let client = if let Some(url) = &cli.url {
        Client::tcp(url).map_err(CliError::from)?
    } else {
        Client::unix(
            cli.socket
                .clone()
                .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET)),
        )
    };
    Ok(client.with_timeout(cli.timeout))
}

async fn status(cli: &Cli, client: &Client) -> Result<()> {
    let status = client.system_status().await?;
    if cli.json {
        emit_json(&status)?;
    } else {
        println!(
            "daemon {} (version {}, API {}, instance {})",
            status.status, status.daemon_version, status.api_version, status.instance_id
        );
        println!("transport: {}", transport_description(cli));
    }
    Ok(())
}

async fn list(cli: &Cli, client: &Client) -> Result<()> {
    let applications = all_applications(client).await?;
    let mut rows = Vec::with_capacity(applications.len());
    for application in applications {
        let status = client
            .application_status(application.application.id.as_str())
            .await?;
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
        emit_json(&json!({"items": items, "next_cursor": Value::Null}))?;
    } else if rows.is_empty() {
        println!("No applications.");
    } else {
        for (application, status) in rows {
            println!(
                "{}\t{}\tgeneration {}\tdesired replicas {}\t{}",
                application.application.metadata.name,
                application.application.id,
                application.generation,
                desired_replicas(&application),
                status.state,
            );
            if let Some(message) = status.message {
                eprintln!("  {}: {message}", application.application.metadata.name);
            }
            if let Some(infrastructure) = status.infrastructure {
                eprintln!("  ingress: {infrastructure}");
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
        emit_json(&json!({"application": application, "status": status}))?;
    } else {
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
        if let Some(infrastructure) = &status.infrastructure {
            println!("ingress: {infrastructure}");
        }
        println!("desired replicas: {}", desired_replicas(&application));
        for service in &application.application.spec.services {
            let image = match &service.source {
                Source::Image { image } => image,
                Source::Git {
                    repository,
                    reference,
                    ..
                } => {
                    // The resolved service image is shown separately below;
                    // this keeps the source summary useful for Git services.
                    &format!("git:{repository}#{reference}")
                }
            };
            println!(
                "service {}: {} replica(s), image {image}",
                service.name, service.replicas
            );
        }
        for service in &status.services {
            println!(
                "runtime {}: {} / {} running ({}){}",
                service.service,
                service.running_replicas,
                service.desired_replicas,
                service.state,
                service
                    .diagnostic
                    .as_deref()
                    .map_or_else(String::new, |message| format!(" — {message}")),
            );
        }
        if !application.application.spec.routes.is_empty() {
            println!("routes: {}", application.application.spec.routes.len());
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
    }
    Ok(())
}

async fn plan_command(cli: &Cli, client: &Client, args: &ManifestArgs) -> Result<()> {
    let manifest = read_manifest(&args.file).await?;
    let name = manifest_name(&manifest, &args.file)?;
    let (plan, _) = prepare_plan(client, &manifest, &name, args.expected_generation).await?;
    if cli.json {
        emit_json(&plan)?;
    } else {
        render_plan(&plan, &mut io::stdout()).map_err(|error| {
            CliError::new(ErrorKind::General, format!("could not write plan: {error}"))
        })?;
    }
    if plan.plan.is_blocked() {
        return Err(blocked_plan_error(&plan));
    }
    Ok(())
}

async fn apply(cli: &Cli, client: &Client, args: &ApplyArgs) -> Result<()> {
    let manifest = read_manifest(&args.file).await?;
    let name = manifest_name(&manifest, &args.file)?;
    let (plan, existing) = prepare_plan(client, &manifest, &name, args.expected_generation).await?;
    if plan.plan.is_blocked() {
        render_plan_stderr(&plan);
        return Err(blocked_plan_error(&plan));
    }
    render_plan_stderr(&plan);
    confirm(args.yes, &format!("Apply application {name:?}? [y/N] "))?;

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
            emit_json(&accepted)?;
        } else {
            println!(
                "accepted operation {} for application {}",
                accepted.operation_id, accepted.application_id
            );
        }
        return Ok(());
    }
    let operation = wait_for_operation(client, &accepted.operation_id, None).await?;
    if cli.json {
        emit_json(&json!({"accepted": accepted, "operation": operation}))?;
    } else {
        println!("operation {} {}", operation.id, operation.state);
    }
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
    )?;

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
            emit_json(&json!({"accepted": accepted, "volumes_retained": true}))?;
        } else {
            println!(
                "accepted operation {} (named volumes retained)",
                accepted.operation_id
            );
        }
        return Ok(());
    }
    let operation = wait_for_operation(client, &accepted.operation_id, None).await?;
    if cli.json {
        emit_json(&json!({
            "accepted": accepted,
            "operation": operation,
            "volumes_retained": true,
        }))?;
    } else {
        println!(
            "operation {} {} (named volumes retained)",
            operation.id, operation.state
        );
    }
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

async fn secret(cli: &Cli, client: &Client, command: &SecretCommand) -> Result<()> {
    match command {
        SecretCommand::List => list_secrets(cli, client).await,
        SecretCommand::Set(args) => set_secret(cli, client, args).await,
        SecretCommand::Delete(args) => delete_secret(cli, client, args).await,
    }
}

async fn build(cli: &Cli, client: &Client, command: &BuildCommand) -> Result<()> {
    match command {
        BuildCommand::Show { build_id } => {
            let build = client.build(build_id).await?;
            render_build(cli, &build)
        }
        BuildCommand::Operation { operation_id } => {
            let builds = client.operation_builds(operation_id).await?;
            if cli.json {
                emit_json(&builds)
            } else if builds.items.is_empty() {
                println!("No builds for operation {operation_id}.");
                Ok(())
            } else {
                for build in builds.items {
                    render_build_text(&build);
                }
                Ok(())
            }
        }
    }
}

async fn logs(cli: &Cli, client: &Client, args: &LogsArgs) -> Result<()> {
    let application = resolve_application(client, &args.name_or_id).await?;
    let records = client
        .application_logs(
            application.application.id.as_str(),
            &ApplicationLogsOptions {
                since_seconds: args.since_seconds,
                tail: Some(args.tail),
                max_bytes: Some(args.max_bytes),
            },
        )
        .await?;
    if cli.json {
        emit_json(&json!({"items": records}))
    } else if records.is_empty() {
        println!("No logs for {}.", application.application.metadata.name);
        Ok(())
    } else {
        for record in records {
            render_log_text(&record);
        }
        Ok(())
    }
}

fn render_log_text(record: &ContainerLogView) {
    println!(
        "{}\t{}\t{}\t{}",
        record.timestamp, record.service, record.stream, record.display_message
    );
}

fn render_build(cli: &Cli, build: &BuildView) -> Result<()> {
    if cli.json {
        emit_json(build)
    } else {
        render_build_text(build);
        Ok(())
    }
}

fn render_build_text(build: &BuildView) {
    println!(
        "build {}: {} (application {}, operation {}, service {})",
        build.id, build.state, build.application_id, build.operation_id, build.service_name,
    );
    if let Some(commit) = &build.source_commit {
        println!("  source commit: {commit}");
    }
    if let Some(image) = &build.image_digest {
        println!("  image digest: {image}");
    }
    println!("  verified: {}", build.verified);
}

async fn list_secrets(cli: &Cli, client: &Client) -> Result<()> {
    let secrets = all_secrets(client).await?;
    if cli.json {
        emit_json(&json!({"items": secrets}))?;
    } else if secrets.is_empty() {
        println!("No logical secrets.");
    } else {
        for secret in secrets {
            println!(
                "{}\tgeneration {}\tvalue {}\treferences {}",
                secret.name,
                secret.generation,
                if secret.value_is_set { "set" } else { "unset" },
                secret.references.len(),
            );
        }
    }
    Ok(())
}

async fn set_secret(cli: &Cli, client: &Client, args: &SecretSetArgs) -> Result<()> {
    let existing = match client.secret(&args.name).await {
        Ok(secret) => Some(secret),
        Err(ClientError::Api { status, .. }) if status.as_u16() == 404 => None,
        Err(error) => return Err(error.into()),
    };

    let metadata = if args.stdin {
        let value = read_secret_stdin()?;
        if existing.is_some() {
            client.replace_secret(&args.name, value).await?
        } else {
            client.create_secret(&args.name, value).await?
        }
    } else {
        let path = args
            .file
            .as_deref()
            .expect("clap requires either --file or --stdin");
        if existing.is_some() {
            client.replace_secret_file(&args.name, path).await?
        } else {
            client.create_secret_file(&args.name, path).await?
        }
    };
    if cli.json {
        emit_json(&metadata)?;
    } else {
        println!(
            "secret {} {} (generation {})",
            metadata.name,
            if existing.is_some() {
                "replaced"
            } else {
                "created"
            },
            metadata.generation,
        );
    }
    Ok(())
}

async fn delete_secret(cli: &Cli, client: &Client, args: &SecretDeleteArgs) -> Result<()> {
    let metadata = client.secret(&args.name).await?;
    if !metadata.references.is_empty() {
        return Err(CliError::new(
            ErrorKind::Conflict,
            format!(
                "secret {:?} is still referenced by {} application service(s); remove those references first",
                metadata.name,
                metadata.references.len()
            ),
        ));
    }
    confirm(
        args.yes,
        &format!("Delete logical secret {:?}? [y/N] ", metadata.name),
    )?;
    client.delete_secret(&metadata.name).await?;
    if cli.json {
        emit_json(&json!({"deleted": true, "name": metadata.name}))?;
    } else {
        println!("secret {} deleted", metadata.name);
    }
    Ok(())
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
    let mut applications = Vec::new();
    let mut cursor = None;
    let mut seen_cursors = BTreeSet::new();
    for _ in 0..MAX_PAGINATION_PAGES {
        let page: Page<ApplicationView> = client
            .applications_with(&ListApplicationsOptions {
                cursor: cursor.clone(),
                limit: Some(PAGE_SIZE),
            })
            .await?;
        applications.extend(page.items);
        let Some(next_cursor) = page.next_cursor else {
            return Ok(applications);
        };
        if !seen_cursors.insert(next_cursor.clone()) {
            return Err(CliError::new(
                ErrorKind::General,
                "the daemon returned a repeated pagination cursor",
            ));
        }
        cursor = Some(next_cursor);
    }
    Err(CliError::new(
        ErrorKind::General,
        "application pagination exceeded the safety bound",
    ))
}

async fn all_secrets(client: &Client) -> Result<Vec<SecretMetadata>> {
    let mut secrets = Vec::new();
    let mut cursor = None;
    let mut seen_cursors = BTreeSet::new();
    for _ in 0..MAX_PAGINATION_PAGES {
        let page: Page<SecretMetadata> = client
            .secrets_with(&ListSecretsOptions {
                cursor: cursor.clone(),
                limit: Some(PAGE_SIZE),
            })
            .await?;
        secrets.extend(page.items);
        let Some(next_cursor) = page.next_cursor else {
            return Ok(secrets);
        };
        if !seen_cursors.insert(next_cursor.clone()) {
            return Err(CliError::new(
                ErrorKind::General,
                "the daemon returned a repeated secret pagination cursor",
            ));
        }
        cursor = Some(next_cursor);
    }
    Err(CliError::new(
        ErrorKind::General,
        "secret pagination exceeded the safety bound",
    ))
}

async fn find_by_name(client: &Client, name: &str) -> Result<Option<ApplicationView>> {
    let matches = all_applications(client)
        .await?
        .into_iter()
        .filter(|application| application.application.metadata.name == name)
        .collect::<Vec<_>>();
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
        return match client.application(name_or_id).await {
            Ok(application) => Ok(application),
            Err(ClientError::Api { status, .. }) if status.as_u16() == 404 => Err(CliError::new(
                ErrorKind::Input,
                format!("application ID {name_or_id:?} was not found"),
            )),
            Err(error) => Err(error.into()),
        };
    }
    let matches = all_applications(client)
        .await?
        .into_iter()
        .filter(|application| application.application.metadata.name == name_or_id)
        .collect::<Vec<_>>();
    match matches.len() {
        0 => Err(CliError::new(
            ErrorKind::Input,
            format!("application name {name_or_id:?} was not found"),
        )),
        1 => Ok(matches.into_iter().next().expect("length checked")),
        count => Err(CliError::new(
            ErrorKind::Conflict,
            format!("application name {name_or_id:?} matched {count} applications"),
        )),
    }
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

fn report_operation(operation: &OperationView) {
    eprintln!("operation {}: {}", operation.id, operation.state);
    for step in &operation.steps {
        eprintln!(
            "  {:>3} {}: {} (attempt {})",
            step.position, step.action, step.state, step.attempt
        );
        if let Some(message) = &step.error_message {
            eprintln!("      {message}");
        }
    }
}

fn render_operation(cli: &Cli, operation: &OperationView) -> Result<()> {
    if cli.json {
        emit_json(operation)
    } else {
        println!("operation {}: {}", operation.id, operation.state);
        println!(
            "application {} generation {}",
            operation.application_id, operation.generation
        );
        for step in &operation.steps {
            println!("  {} {}: {}", step.position, step.action, step.state);
        }
        if let Some(message) = &operation.error_message {
            eprintln!("diagnostic: {message}");
        }
        Ok(())
    }
}

fn render_plan(plan: &PlanView, output: &mut impl Write) -> io::Result<()> {
    writeln!(
        output,
        "application {} proposed generation {}",
        plan.application_id, plan.proposed_generation
    )?;
    writeln!(
        output,
        "{} action(s), {} mutation(s), {} destructive action(s), {} blocking conflict(s)",
        plan.plan.summary.action_count,
        plan.plan.summary.mutation_count,
        plan.plan.summary.destructive_count,
        plan.plan.summary.blocking_conflicts,
    )?;
    for action in &plan.plan.actions {
        writeln!(
            output,
            "  {:>3} [{:?}] {} ({})",
            action.sequence,
            action.risk,
            action.human_description(),
            reason_text(&action.reason),
        )?;
    }
    for diagnostic in &plan.plan.diagnostics {
        writeln!(
            output,
            "  diagnostic {} [{}]: {}{}",
            diagnostic.code,
            diagnostic.resource,
            diagnostic.message,
            if diagnostic.blocking {
                " (blocking)"
            } else {
                ""
            },
        )?;
    }
    Ok(())
}

fn render_plan_stderr(plan: &PlanView) {
    eprintln!(
        "plan: {} action(s), {} mutation(s), {} destructive action(s), {} blocking conflict(s)",
        plan.plan.summary.action_count,
        plan.plan.summary.mutation_count,
        plan.plan.summary.destructive_count,
        plan.plan.summary.blocking_conflicts,
    );
    for action in &plan.plan.actions {
        eprintln!(
            "  {:>3} [{:?}] {} ({})",
            action.sequence,
            action.risk,
            action.human_description(),
            reason_text(&action.reason),
        );
    }
    for diagnostic in &plan.plan.diagnostics {
        eprintln!(
            "  diagnostic {} [{}]: {}{}",
            diagnostic.code,
            diagnostic.resource,
            diagnostic.message,
            if diagnostic.blocking {
                " (blocking)"
            } else {
                ""
            },
        );
    }
}

fn blocked_plan_error(plan: &PlanView) -> CliError {
    let codes = plan
        .plan
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.blocking)
        .map(|diagnostic| diagnostic.code.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    CliError::new(
        ErrorKind::Conflict,
        if codes.is_empty() {
            "plan contains blocking diagnostics".into()
        } else {
            format!("plan is blocked ({codes})")
        },
    )
    .with_details(json!({"plan": plan}))
}

fn reason_text(reason: &impl fmt::Debug) -> String {
    format!("{reason:?}")
}

fn confirm(yes: bool, prompt: &str) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        return Err(CliError::new(
            ErrorKind::Input,
            "confirmation is required in a non-interactive terminal; pass --yes",
        ));
    }
    eprint!("{prompt}");
    io::stderr().flush().map_err(|error| {
        CliError::new(
            ErrorKind::General,
            format!("could not write prompt: {error}"),
        )
    })?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer).map_err(|error| {
        CliError::new(
            ErrorKind::General,
            format!("could not read confirmation: {error}"),
        )
    })?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        Err(CliError::new(
            ErrorKind::Input,
            "operation was not confirmed",
        ))
    }
}

fn read_secret_stdin() -> Result<Vec<u8>> {
    if io::stdin().is_terminal() {
        return Err(CliError::new(
            ErrorKind::Input,
            "secret input must be supplied through a pipe or with --file; plaintext command arguments are not accepted",
        ));
    }
    let mut value = Vec::new();
    io::stdin()
        .take((MAX_SECRET_BYTES + 1) as u64)
        .read_to_end(&mut value)
        .map_err(|error| {
            CliError::new(
                ErrorKind::Input,
                format!("could not read secret input: {error}"),
            )
        })?;
    if value.is_empty() || value.len() > MAX_SECRET_BYTES {
        return Err(CliError::new(
            ErrorKind::Input,
            format!("secret input must be between 1 and {MAX_SECRET_BYTES} bytes"),
        ));
    }
    Ok(value)
}

async fn read_manifest(path: &Path) -> Result<String> {
    let metadata = fs::metadata(path).await.map_err(|error| {
        CliError::new(
            ErrorKind::Input,
            format!("could not read manifest {}: {error}", path.display()),
        )
    })?;
    if !metadata.is_file() {
        return Err(CliError::new(
            ErrorKind::Input,
            format!("manifest path {} is not a regular file", path.display()),
        ));
    }
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Err(CliError::new(
            ErrorKind::Input,
            format!(
                "manifest {} exceeds the {}-byte input limit",
                path.display(),
                MAX_MANIFEST_BYTES
            ),
        ));
    }
    let bytes = fs::read(path).await.map_err(|error| {
        CliError::new(
            ErrorKind::Input,
            format!("could not read manifest {}: {error}", path.display()),
        )
    })?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(CliError::new(
            ErrorKind::Input,
            format!("manifest {} exceeds the input limit", path.display()),
        ));
    }
    String::from_utf8(bytes).map_err(|_| {
        CliError::new(
            ErrorKind::Input,
            format!("manifest {} is not valid UTF-8", path.display()),
        )
    })
}

fn manifest_name(manifest: &str, path: &Path) -> Result<String> {
    piqueld_client::application_name_from_toml(manifest)
        .map_err(|errors| manifest_validation_error(path, &errors))
}

fn manifest_validation_error(path: &Path, errors: &ValidationErrors) -> CliError {
    CliError::new(
        ErrorKind::Input,
        format!("manifest {} failed validation: {errors}", path.display()),
    )
    .with_details(json!({"errors": errors}))
}

async fn retry_transport<T, F, Fut>(mut request: F) -> std::result::Result<T, ClientError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::result::Result<T, ClientError>>,
{
    let result = request().await;
    if matches!(&result, Err(ClientError::Transport { .. })) {
        time::sleep(Duration::from_millis(25)).await;
        request().await
    } else {
        result
    }
}

fn idempotency_key() -> String {
    format!("piquelctl-{}", Uuid::now_v7().simple())
}

fn transport_description(cli: &Cli) -> String {
    if let Some(url) = &cli.url {
        format!("loopback TCP {url}")
    } else {
        let socket = cli.socket.as_deref().map_or_else(
            || DEFAULT_SOCKET.to_owned(),
            |path| path.to_string_lossy().into_owned(),
        );
        format!("Unix socket {socket}")
    }
}

fn desired_replicas(application: &ApplicationView) -> u32 {
    application
        .application
        .spec
        .services
        .iter()
        .map(|service| u32::from(service.replicas))
        .sum()
}

fn looks_like_application_id(value: &str) -> bool {
    (8..=64).contains(&value.len())
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .last()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn parse_generation(value: &str) -> std::result::Result<u64, String> {
    let generation = value
        .parse::<u64>()
        .map_err(|_| "generation must be a positive integer".to_owned())?;
    (generation > 0)
        .then_some(generation)
        .ok_or_else(|| "generation must be a positive integer".to_owned())
}

fn parse_duration(value: &str) -> std::result::Result<Duration, String> {
    let (number, unit) = if let Some(value) = value.strip_suffix("ms") {
        (value, "ms")
    } else if let Some(value) = value.strip_suffix('s') {
        (value, "s")
    } else if let Some(value) = value.strip_suffix('m') {
        (value, "m")
    } else if let Some(value) = value.strip_suffix('h') {
        (value, "h")
    } else {
        (value, "s")
    };
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("timeout must be an integer duration such as 500ms, 30s, or 2m".to_owned());
    }
    let number = number
        .parse::<u64>()
        .map_err(|_| "timeout is too large".to_owned())?;
    let duration = match unit {
        "ms" => Duration::from_millis(number),
        "s" => Duration::from_secs(number),
        "m" => Duration::from_secs(
            number
                .checked_mul(60)
                .ok_or_else(|| "timeout is too large".to_owned())?,
        ),
        "h" => Duration::from_secs(
            number
                .checked_mul(60 * 60)
                .ok_or_else(|| "timeout is too large".to_owned())?,
        ),
        _ => unreachable!("duration suffix is selected above"),
    };
    if duration.is_zero() {
        return Err("timeout must be greater than zero".to_owned());
    }
    Ok(duration)
}

fn format_duration(duration: Duration) -> String {
    if duration.as_millis().is_multiple_of(1000) {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

fn emit_json<T: Serialize>(value: &T) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, value).map_err(|error| {
        CliError::new(
            ErrorKind::General,
            format!("could not encode JSON: {error}"),
        )
    })?;
    writeln!(output).map_err(|error| {
        CliError::new(ErrorKind::General, format!("could not write JSON: {error}"))
    })?;
    Ok(())
}

fn finish_error(cli: &Cli, error: CliError) -> ExitCode {
    if cli.json {
        eprintln!(
            "piquelctl: {}{}{}{}",
            error.message,
            error
                .api_code
                .as_deref()
                .map_or_else(String::new, |code| format!("; API code {code}")),
            error
                .request_id
                .as_deref()
                .map_or_else(String::new, |id| format!("; request ID {id}")),
            error
                .details
                .as_ref()
                .map_or_else(String::new, |details| format!("; details {details}")),
        );
    } else {
        eprintln!("piquelctl: {}", error.message);
        if let Some(code) = error.api_code {
            eprintln!("API code: {code}");
        }
        if let Some(request_id) = error.request_id {
            eprintln!("request ID: {request_id}");
        }
        if let Some(details) = error.details {
            eprintln!("details: {details}");
        }
    }
    ExitCode::from(error.kind.exit_code())
}

fn terminal_operation(state: &str) -> bool {
    matches!(
        state.to_ascii_lowercase().as_str(),
        "succeeded" | "completed" | "failed" | "cancelled" | "canceled"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_covers_the_initial_command_surface() {
        let cases = [
            vec!["piquelctl", "status"],
            vec!["piquelctl", "list"],
            vec!["piquelctl", "show", "notes"],
            vec!["piquelctl", "plan", "--file", "application.toml"],
            vec!["piquelctl", "apply", "--file", "application.toml", "--yes"],
            vec!["piquelctl", "delete", "notes", "--yes", "--no-wait"],
            vec!["piquelctl", "operation", "operation-01", "--no-wait"],
            vec!["piquelctl", "secret", "list"],
            vec!["piquelctl", "secret", "set", "database-password", "--stdin"],
            vec![
                "piquelctl",
                "secret",
                "set",
                "database-password",
                "--file",
                "secret.txt",
            ],
            vec![
                "piquelctl",
                "secret",
                "delete",
                "database-password",
                "--yes",
            ],
            vec!["piquelctl", "build", "show", "build-01"],
            vec!["piquelctl", "build", "operation", "operation-01"],
            vec!["piquelctl", "logs", "notes"],
        ];
        for arguments in cases {
            Cli::try_parse_from(arguments).expect("command parses");
        }
    }

    #[test]
    fn parser_rejects_conflicting_transports_and_invalid_timeout() {
        assert!(
            Cli::try_parse_from([
                "piquelctl",
                "--socket",
                "/tmp/piqueld.sock",
                "--url",
                "http://127.0.0.1:8080/",
                "status",
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["piquelctl", "--timeout", "zero", "status"]).is_err());
        assert!(Cli::try_parse_from(["piquelctl", "--timeout", "0s", "status"]).is_err());
        assert!(Cli::try_parse_from(["piquelctl", "plan", "status"]).is_err());
        assert!(Cli::try_parse_from(["piquelctl", "secret", "set", "database-password"]).is_err());
        assert!(
            Cli::try_parse_from([
                "piquelctl",
                "secret",
                "set",
                "database-password",
                "--stdin",
                "--file",
                "secret.txt",
            ])
            .is_err()
        );
    }

    #[test]
    fn timeout_parser_accepts_bounded_units() {
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_mins(2));
        assert_eq!(parse_duration("1").unwrap(), Duration::from_secs(1));
    }

    #[test]
    fn application_ids_are_distinguished_from_names() {
        assert!(looks_like_application_id("app-notes-01"));
        assert!(!looks_like_application_id("notes"));
        assert!(!looks_like_application_id("APP-NOTES"));
        assert!(!looks_like_application_id("--------"));
    }
}
