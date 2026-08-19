//! Safe, small operator command-line client for the Plan 06 piqueld API.

use clap::{Args, Parser, Subcommand};
use piqueld_client::{
    ApplicationLogsOptions, ApplicationView, BuildView, Client, ClientError, ContainerLogView,
    ListApplicationsOptions, ListSecretsOptions, MAX_STATE_ARCHIVE_BYTES, OperationView, Page,
    PlanView, SecretMetadata, Source, StateExportMode, ValidationErrors,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    env, fmt,
    fmt::Write as _,
    fs::{self, OpenOptions},
    future::Future,
    io::{self, IsTerminal, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};
use tokio::{fs as async_fs, signal, time};
use uuid::Uuid;
use zeroize::Zeroize;

const DEFAULT_SOCKET: &str = "/run/piqueld/piqueld.sock";
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const MAX_SECRET_BYTES: usize = 500 * 1024;
const MAX_PAGINATION_PAGES: usize = 10_000;
const PAGE_SIZE: u16 = 100;
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Essential commands for inspecting and operating Plan 06 applications.
#[derive(Debug, Parser)]
#[command(
    name = "piquelctl",
    version,
    about = "Operate a local piqueld control plane"
)]
struct Cli {
    /// Connection profile from the optional profiles file.
    #[arg(
        long,
        global = true,
        env = "PIQUELD_PROFILE",
        default_value = "default"
    )]
    profile: String,

    /// Profiles TOML path. The default is `$XDG_CONFIG_HOME/piqueld/profiles.toml`.
    #[arg(long, global = true, env = "PIQUELD_PROFILES_FILE")]
    profiles_file: Option<PathBuf>,

    /// Unix socket path. The default is the daemon's local socket.
    #[arg(
        long,
        global = true,
        env = "PIQUELD_SOCKET",
        value_name = "PATH",
        conflicts_with = "url"
    )]
    socket: Option<PathBuf>,

    /// Explicit loopback or Tailscale HTTP endpoint.
    #[arg(
        long,
        global = true,
        env = "PIQUELD_URL",
        value_name = "URL",
        conflicts_with = "socket"
    )]
    url: Option<String>,

    /// Protected file containing an administrative bearer token.
    #[arg(
        long,
        global = true,
        env = "PIQUELD_TOKEN_FILE",
        conflicts_with = "token_env"
    )]
    token_file: Option<PathBuf>,

    /// Name of the environment variable containing an administrative bearer token.
    #[arg(long, global = true, conflicts_with = "token_file")]
    token_env: Option<String>,

    /// Bound for each request and for the complete command wait.
    #[arg(long, global = true, default_value = "30s", value_parser = parse_duration)]
    timeout: Duration,

    /// Emit only the command's documented JSON result on stdout.
    #[arg(long, global = true)]
    json: bool,

    /// Stable output format (`human` or `json`); for legacy export this also
    /// accepts the historical destination path.
    #[arg(long, global = true, value_name = "FORMAT|PATH")]
    output: Option<String>,

    /// Suppress progress and successful human output.
    #[arg(long, short, global = true)]
    quiet: bool,

    /// Never prompt; explicit confirmation flags are still required.
    #[arg(long, global = true)]
    noninteractive: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfilesFile {
    default: Option<String>,
    #[serde(default)]
    profiles: std::collections::BTreeMap<String, Profile>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    unix_socket: Option<PathBuf>,
    url: Option<String>,
    token_file: Option<PathBuf>,
    token_env: Option<String>,
}

impl Cli {
    fn structured_json(&self) -> bool {
        self.output.as_deref() == Some("json")
    }

    fn json_output(&self) -> bool {
        self.json || self.structured_json()
    }

    fn validate_output(&self) -> Result<()> {
        if let Some(output) = &self.output
            && !matches!(output.as_str(), "human" | "json")
            && !matches!(&self.command, Command::Export(_))
        {
            return Err(CliError::new(
                ErrorKind::Input,
                "--output must be human or json",
            ));
        }
        Ok(())
    }
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
    /// Export one application manifest or the complete binary state archive.
    Export(ExportArgs),
    /// Confirm and transactionally import a complete binary state archive.
    Import(ImportArgs),
    /// Advanced grouped application commands. Legacy flat commands remain available.
    Application {
        #[command(subcommand)]
        command: ApplicationCommand,
    },
    /// Export or replace complete control-plane state.
    State {
        #[command(subcommand)]
        command: StateCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ApplicationCommand {
    /// List applications and their persisted generations.
    List,
    /// Show desired and observed application status.
    Show { name: String },
    /// Preview a manifest without changing desired state.
    Plan(ManifestArgs),
    /// Replace desired state and optionally wait for reconciliation.
    Apply(ApplyArgs),
    /// Export one canonical application manifest.
    Export {
        name: String,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        include_resolved: bool,
        #[arg(long)]
        force: bool,
    },
    /// Delete an application while retaining named volumes.
    Delete {
        name: String,
        #[arg(long, value_parser = parse_generation)]
        expected_generation: Option<u64>,
        #[arg(long)]
        yes: bool,
        /// Kept as an explicit rejected option; runtime force deletion is unsupported.
        #[arg(long)]
        force: bool,
    },
    /// Read or follow sanitized application logs.
    Logs(ApplicationLogsArgs),
}

#[derive(Debug, Subcommand)]
enum OperationCommand {
    /// Watch an operation through SSE with polling fallback.
    Watch {
        id: String,
        #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u64).range(1..))]
        poll_seconds: u64,
    },
}

#[derive(Debug, Subcommand)]
enum StateCommand {
    /// Export a portable or encrypted state archive.
    Export {
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "portable")]
        mode: ExportMode,
        #[arg(long)]
        force: bool,
    },
    /// Transactionally replace state after explicit confirmation.
    Import {
        archive: PathBuf,
        #[arg(long)]
        replace: bool,
        #[arg(long)]
        yes: bool,
    },
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

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum ExportMode {
    Portable,
    Encrypted,
}

#[derive(Debug, Args)]
struct ExportArgs {
    /// Application name or ID; omit to export the complete control-plane state.
    #[arg(long)]
    application: Option<String>,

    /// Explicit output file. `-` means stdout.
    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,

    /// Secret mode for complete state exports.
    #[arg(long, value_enum, default_value = "portable")]
    mode: ExportMode,

    /// Include resolved source metadata in application exports.
    #[arg(long)]
    include_resolved: bool,

    /// Allow replacing an existing output file.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct ImportArgs {
    /// State archive to import.
    #[arg(value_name = "PATH")]
    file: PathBuf,

    /// Skip the destructive replacement confirmation prompt.
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct ApplicationLogsArgs {
    /// Application name or stable ID.
    name: String,
    #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(0..=86_400))]
    since_seconds: u64,
    #[arg(long, default_value_t = 200, value_parser = clap::value_parser!(u16).range(1..=1_000))]
    tail: u16,
    #[arg(long, default_value_t = 256 * 1024, value_parser = clap::value_parser!(u32).range(1..=1_048_576))]
    max_bytes: u32,
    #[arg(long)]
    follow: bool,
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
    /// Stable operation ID for the legacy flat form.
    operation_id: Option<String>,

    #[command(subcommand)]
    command: Option<OperationCommand>,

    /// Fetch once instead of waiting for a terminal state.
    #[arg(long)]
    no_wait: bool,
}

#[derive(Clone, Copy, Debug)]
enum ErrorKind {
    General,
    Input,
    Authentication,
    Conflict,
    Unavailable,
    Operation,
    Interrupted,
    Refused,
}

impl ErrorKind {
    const fn exit_code(self) -> u8 {
        match self {
            Self::General => 1,
            Self::Input => 3,
            Self::Authentication => 4,
            Self::Conflict => 5,
            Self::Unavailable => 6,
            Self::Operation => 7,
            Self::Interrupted => 8,
            Self::Refused => 9,
        }
    }

    const fn legacy_exit_code(self) -> u8 {
        match self {
            Self::General => 1,
            Self::Input | Self::Refused => 2,
            Self::Authentication | Self::Unavailable => 4,
            Self::Conflict => 3,
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
                    401 | 403 => ErrorKind::Authentication,
                    409 | 412 => ErrorKind::Conflict,
                    502..=504 => ErrorKind::Unavailable,
                    _ => ErrorKind::General,
                };
                let message = match kind {
                    ErrorKind::Authentication => "authentication or authorization failed",
                    ErrorKind::Conflict => "the request conflicts with current server state",
                    ErrorKind::Input => "the daemon rejected the request as invalid",
                    ErrorKind::Unavailable => "the requested daemon capability is unavailable",
                    _ => "the daemon rejected the request",
                };
                Self::new(kind, message).api(error.code, error.request_id, error.details)
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
    cli.validate_output()?;
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
        Command::Export(args) => export_command(cli, &client, args).await,
        Command::Import(args) => import_command(cli, &client, args).await,
        Command::Application { command } => application(cli, &client, command).await,
        Command::State { command } => state(cli, &client, command).await,
    }
}

fn build_client(cli: &Cli) -> Result<Client> {
    let profiles_path = cli.profiles_file.clone().or_else(default_profiles_path);
    let profiles = profiles_path
        .as_deref()
        .filter(|path| path.exists())
        .map(read_profiles)
        .transpose()?
        .unwrap_or_default();
    let selected_name = if cli.profile == "default" {
        profiles.default.as_deref().unwrap_or("default")
    } else {
        &cli.profile
    };
    let profile = profiles.profiles.get(selected_name);
    if (cli.profile != "default" || profiles.default.is_some()) && profile.is_none() {
        return Err(CliError::new(
            ErrorKind::Input,
            format!("connection profile {selected_name:?} was not found"),
        ));
    }

    let (socket, url) = if cli.socket.is_some() || cli.url.is_some() {
        (cli.socket.clone(), cli.url.clone())
    } else {
        (
            profile.and_then(|profile| profile.unix_socket.clone()),
            profile.and_then(|profile| profile.url.clone()),
        )
    };
    if socket.is_some() && url.is_some() {
        return Err(CliError::new(
            ErrorKind::Input,
            "connection profile must specify exactly one transport",
        ));
    }
    let mut client = if let Some(url) = url {
        Client::tcp(&url).map_err(|_| {
            CliError::new(
                ErrorKind::Input,
                "HTTP endpoint must be an http:// loopback or Tailscale origin without credentials, path, query, or fragment",
            )
        })?
    } else {
        Client::unix(socket.unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET)))
    }
    .with_timeout(cli.timeout);

    let (token_file, token_env) = if cli.token_file.is_some() || cli.token_env.is_some() {
        (cli.token_file.clone(), cli.token_env.as_deref())
    } else {
        (
            profile.and_then(|profile| profile.token_file.clone()),
            profile.and_then(|profile| profile.token_env.as_deref()),
        )
    };
    let token_env = token_env.unwrap_or("PIQUELD_TOKEN");
    if token_env.is_empty() {
        return Err(CliError::new(
            ErrorKind::Input,
            "token environment variable name must not be empty",
        ));
    }
    let mut token = if let Some(path) = token_file {
        Some(read_private_text(&path, 4096, "token")?)
    } else {
        env::var(token_env).ok()
    };
    if let Some(value) = token.as_mut() {
        let trimmed = value.trim_end_matches(['\r', '\n']).to_owned();
        value.zeroize();
        if trimmed.is_empty()
            || trimmed.len() > 4096
            || trimmed.bytes().any(|byte| byte.is_ascii_control())
        {
            let mut trimmed = trimmed;
            trimmed.zeroize();
            return Err(CliError::new(
                ErrorKind::Input,
                "administrative token has an invalid size or format",
            ));
        }
        client = client.with_bearer_token(trimmed).map_err(|_| {
            CliError::new(
                ErrorKind::Input,
                "administrative token has an invalid format",
            )
        })?;
    }
    Ok(client)
}

async fn status(cli: &Cli, client: &Client) -> Result<()> {
    let status = client.system_status().await?;
    if cli.structured_json() {
        let capabilities = client.capabilities().await?;
        emit_json_envelope(
            "status",
            &json!({
                "status": status,
                "capabilities": capabilities,
            }),
        )?;
    } else if cli.json {
        emit_json(&status)?;
    } else {
        let capabilities = client.capabilities().await?;
        println!(
            "daemon {} (version {}, API {}, instance {})",
            status.status, status.daemon_version, status.api_version, status.instance_id
        );
        println!("transport: {}", transport_description(cli));
        println!(
            "capabilities: persistence={} resolution={} observation={} execution={} secrets={}",
            capabilities.persistence,
            capabilities.source_resolution,
            capabilities.runtime_observation,
            capabilities.runtime_execution,
            capabilities.secret_management
        );
        if let Some(reason) = capabilities.reason {
            println!("diagnostic: {reason}");
        }
    }
    Ok(())
}

async fn application(cli: &Cli, client: &Client, command: &ApplicationCommand) -> Result<()> {
    match command {
        ApplicationCommand::List => list(cli, client).await,
        ApplicationCommand::Show { name } => show(cli, client, name).await,
        ApplicationCommand::Plan(args) => plan_command(cli, client, args).await,
        ApplicationCommand::Apply(args) => apply(cli, client, args).await,
        ApplicationCommand::Export {
            name,
            file,
            include_resolved,
            force,
        } => {
            let application = resolve_application(client, name).await?;
            let document = client
                .export_application(application.application.id.as_str(), *include_resolved)
                .await?;
            let output = if cli.json_output() && file.is_none() {
                None
            } else {
                write_text_output(file.as_deref(), &document, *force)?
            };
            if cli.structured_json() {
                emit_json_envelope(
                    "application_export",
                    &json!({
                        "name": name,
                        "bytes": document.len(),
                        "output": output,
                        "include_resolved": include_resolved,
                        "toml": (file.is_none()).then_some(&document),
                    }),
                )?;
            } else if cli.json {
                emit_json(&json!({
                    "kind": "application",
                    "application_id": application.application.id,
                    "bytes": document.len(),
                    "output": output,
                }))?;
            } else if let Some(path) = output
                && !cli.quiet
            {
                eprintln!("exported application manifest to {}", path.display());
            }
            Ok(())
        }
        ApplicationCommand::Delete {
            name,
            expected_generation,
            yes,
            force,
        } => {
            if *force {
                return Err(CliError::new(
                    ErrorKind::Refused,
                    "force deletion is unsupported; named volumes are always retained",
                ));
            }
            delete(
                cli,
                client,
                &DeleteArgs {
                    name_or_id: name.clone(),
                    expected_generation: *expected_generation,
                    yes: *yes,
                    no_wait: false,
                },
            )
            .await
        }
        ApplicationCommand::Logs(args) => application_logs(cli, client, args).await,
    }
}

async fn state(cli: &Cli, client: &Client, command: &StateCommand) -> Result<()> {
    match command {
        StateCommand::Export { file, mode, force } => {
            let args = ExportArgs {
                application: None,
                file: file.clone(),
                mode: *mode,
                include_resolved: false,
                force: *force,
            };
            export_command(cli, client, &args).await
        }
        StateCommand::Import {
            archive,
            replace,
            yes,
        } => {
            if !replace {
                return Err(CliError::new(
                    ErrorKind::Refused,
                    "state import replaces control-plane state; pass --replace explicitly",
                ));
            }
            import_command(
                cli,
                client,
                &ImportArgs {
                    file: archive.clone(),
                    yes: *yes,
                },
            )
            .await
        }
    }
}

async fn application_logs(cli: &Cli, client: &Client, args: &ApplicationLogsArgs) -> Result<()> {
    let application = resolve_application(client, &args.name).await?;
    let options = ApplicationLogsOptions {
        since_seconds: Some(args.since_seconds),
        tail: Some(args.tail),
        max_bytes: Some(args.max_bytes),
    };
    if args.follow {
        follow_logs(cli, client, application.application.id.as_str(), &options).await
    } else {
        let logs = client
            .application_logs(application.application.id.as_str(), &options)
            .await?;
        if cli.structured_json() {
            emit_json_envelope("application_logs", &logs)?;
        } else if cli.json {
            emit_json(&logs)?;
        } else if !cli.quiet {
            for log in logs {
                println!(
                    "{} {} {}: {}",
                    log.timestamp, log.service, log.stream, log.display_message
                );
            }
        }
        Ok(())
    }
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
    if cli.structured_json() {
        let items = rows
            .iter()
            .map(|(application, status)| {
                json!({
                    "application": application,
                    "status": status,
                })
            })
            .collect::<Vec<_>>();
        emit_json_envelope(
            "application_list",
            &json!({"items": items, "next_cursor": Value::Null}),
        )?;
    } else if cli.json {
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
    if cli.structured_json() {
        emit_json_envelope(
            "application_show",
            &json!({"application": application, "status": status}),
        )?;
    } else if cli.json {
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
    if cli.structured_json() {
        emit_json_envelope("application_plan", &plan)?;
    } else if cli.json {
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
    confirm_with_mode(
        cli,
        args.yes,
        &format!("Apply application {name:?}? [y/N] "),
    )?;

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
        if cli.structured_json() {
            emit_json_envelope("application_apply", &accepted)?;
        } else if cli.json {
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
    if cli.structured_json() {
        emit_json_envelope(
            "application_apply",
            &json!({"accepted": accepted, "operation": operation}),
        )?;
    } else if cli.json {
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
    confirm_with_mode(
        cli,
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
        if cli.structured_json() {
            emit_json_envelope(
                "application_delete",
                &json!({"accepted": accepted, "volumes_retained": true}),
            )?;
        } else if cli.json {
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
    if cli.structured_json() {
        emit_json_envelope(
            "application_delete",
            &json!({
                "accepted": accepted,
                "operation": operation,
                "volumes_retained": true,
            }),
        )?;
    } else if cli.json {
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
    if let Some(OperationCommand::Watch { id, poll_seconds }) = &args.command {
        let operation = watch_operation(cli, client, id, *poll_seconds).await?;
        if cli.structured_json() {
            emit_json_envelope("operation_watch", &operation)?;
        } else if cli.json {
            emit_json(&operation)?;
        } else if !cli.quiet {
            println!("operation {} {}", operation.id, operation.state);
        }
        return Ok(());
    }
    let operation_id = args.operation_id.as_deref().ok_or_else(|| {
        CliError::new(
            ErrorKind::Input,
            "operation requires an ID or the `watch` subcommand",
        )
    })?;
    let initial = client.operation(operation_id).await?;
    if args.no_wait {
        render_operation(cli, &initial)?;
        return Ok(());
    }
    let operation = wait_for_operation(client, operation_id, Some(initial)).await?;
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
            if cli.structured_json() {
                emit_json_envelope("build_operation", &builds)
            } else if cli.json {
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
    if cli.structured_json() {
        emit_json_envelope("logs", &json!({"items": records}))
    } else if cli.json {
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
    if cli.structured_json() {
        emit_json_envelope("build_show", build)
    } else if cli.json {
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
    if cli.structured_json() {
        emit_json_envelope("secret_list", &json!({"items": secrets}))?;
    } else if cli.json {
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

async fn export_command(cli: &Cli, client: &Client, args: &ExportArgs) -> Result<()> {
    if args.application.is_some() {
        return export_application_command(cli, client, args).await;
    }
    if args.include_resolved {
        return Err(CliError::new(
            ErrorKind::Input,
            "--include-resolved requires --application",
        ));
    }
    let destination = export_destination(cli, args);
    if cli.json_output() && destination.is_none() {
        return Err(CliError::new(
            ErrorKind::Input,
            "binary state export with --json requires --output",
        ));
    }
    if destination.is_none() && io::stdout().is_terminal() {
        return Err(CliError::new(
            ErrorKind::Input,
            "refusing to write a binary state archive to a terminal; pass --output",
        ));
    }
    let archive = client
        .export_state(match args.mode {
            ExportMode::Portable => StateExportMode::Portable,
            ExportMode::Encrypted => StateExportMode::Encrypted,
        })
        .await?;
    let archive_digest = digest(&archive);
    let output = write_binary_output(destination, &archive, args.force)?;
    if cli.structured_json() {
        emit_json_envelope(
            "state_export",
            &json!({
                "kind": "state",
                "mode": match args.mode { ExportMode::Portable => "portable", ExportMode::Encrypted => "encrypted" },
                "archive_digest": archive_digest,
                "bytes": archive.len(),
                "output": output,
            }),
        )?;
    } else if cli.json {
        emit_json(&json!({
            "kind": "state",
            "mode": match args.mode { ExportMode::Portable => "portable", ExportMode::Encrypted => "encrypted" },
            "archive_digest": archive_digest,
            "bytes": archive.len(),
            "output": output,
        }))?;
    } else if let Some(output) = output {
        eprintln!(
            "exported state archive to {} ({archive_digest})",
            output.display()
        );
    } else {
        eprintln!("state archive: {archive_digest}");
    }
    Ok(())
}

async fn export_application_command(cli: &Cli, client: &Client, args: &ExportArgs) -> Result<()> {
    if !matches!(args.mode, ExportMode::Portable) {
        return Err(CliError::new(
            ErrorKind::Input,
            "--mode applies only to complete state exports",
        ));
    }
    let application = resolve_application(
        client,
        args.application.as_deref().expect("application is present"),
    )
    .await?;
    let document = client
        .export_application(application.application.id.as_str(), args.include_resolved)
        .await?;
    let destination = export_destination(cli, args);
    let output = if cli.json_output() && destination.is_none() {
        None
    } else {
        write_text_output(destination, &document, args.force)?
    };
    if cli.structured_json() {
        emit_json_envelope(
            "application_export",
            &json!({
                "kind": "application",
                "application_id": application.application.id,
                "bytes": document.len(),
                "output": output,
                "toml": (destination.is_none()).then_some(&document),
            }),
        )?;
    } else if cli.json {
        emit_json(&json!({
            "kind": "application",
            "application_id": application.application.id,
            "bytes": document.len(),
            "output": output,
            "toml": (destination.is_none()).then_some(&document),
        }))?;
    } else if let Some(output) = output
        && !cli.quiet
    {
        eprintln!("exported application manifest to {}", output.display());
    }
    Ok(())
}

fn export_destination<'a>(cli: &'a Cli, args: &'a ExportArgs) -> Option<&'a Path> {
    args.file.as_deref().or_else(|| {
        cli.output
            .as_deref()
            .filter(|value| !matches!(*value, "human" | "json"))
            .map(Path::new)
    })
}

async fn import_command(cli: &Cli, client: &Client, args: &ImportArgs) -> Result<()> {
    let archive = read_archive(&args.file)?;
    let archive_digest = digest(&archive);
    let confirmation = client.prepare_state_import(&archive_digest).await?;
    if confirmation.archive_digest != archive_digest {
        return Err(CliError::new(
            ErrorKind::General,
            "daemon confirmation digest did not match the archive",
        ));
    }
    confirm_with_mode(
        cli,
        args.yes,
        &format!(
            "Replace all control-plane state with {} ({})? [y/N] ",
            args.file.display(),
            archive_digest
        ),
    )?;
    let mut token = confirmation.token;
    let result = client.import_state(archive, &token).await;
    token.zeroize();
    let result = result?;
    if cli.structured_json() {
        emit_json_envelope("state_import", &result)?;
    } else if cli.json {
        emit_json(&result)?;
    } else if !cli.quiet {
        println!(
            "imported {} application(s) and {} secret(s); operation {}",
            result.applications_imported, result.secrets_imported, result.operation_id
        );
        if !result.dependencies.missing_secret_values.is_empty() {
            eprintln!(
                "missing secret values: {}",
                result.dependencies.missing_secret_values.join(", ")
            );
        }
        if !result.dependencies.retained_volumes_to_verify.is_empty() {
            eprintln!(
                "retained volumes to verify: {}",
                result.dependencies.retained_volumes_to_verify.join(", ")
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
    if cli.structured_json() {
        emit_json_envelope("secret_set", &metadata)?;
    } else if cli.json {
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
    confirm_with_mode(
        cli,
        args.yes,
        &format!("Delete logical secret {:?}? [y/N] ", metadata.name),
    )?;
    client.delete_secret(&metadata.name).await?;
    if cli.structured_json() {
        emit_json_envelope(
            "secret_delete",
            &json!({"deleted": true, "name": metadata.name}),
        )?;
    } else if cli.json {
        emit_json(&json!({"deleted": true, "name": metadata.name}))?;
    } else {
        println!("secret {} deleted", metadata.name);
    }
    Ok(())
}

fn read_archive(path: &Path) -> Result<Vec<u8>> {
    read_bounded_file(path, MAX_STATE_ARCHIVE_BYTES as u64, "state archive")
}

fn read_profiles(path: &Path) -> Result<ProfilesFile> {
    let bytes = read_bounded_file(path, 1024 * 1024, "profiles file")?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| CliError::new(ErrorKind::Input, "profiles file is not valid UTF-8"))?;
    toml::from_str(text)
        .map_err(|_| CliError::new(ErrorKind::Input, "profiles file is not valid TOML"))
}

fn default_profiles_path() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|base| base.join("piqueld/profiles.toml"))
}

fn read_private_text(path: &Path, maximum: usize, label: &str) -> Result<String> {
    let descriptor = rustix::fs::openat2(
        rustix::fs::CWD,
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
        rustix::fs::ResolveFlags::NO_SYMLINKS | rustix::fs::ResolveFlags::NO_MAGICLINKS,
    )
    .map_err(|_| CliError::new(ErrorKind::Input, format!("cannot read {label} file")))?;
    let mut file = fs::File::from(descriptor);
    let opened = file
        .metadata()
        .map_err(|_| CliError::new(ErrorKind::Input, format!("cannot read {label} file")))?;
    let path_metadata = fs::symlink_metadata(path)
        .map_err(|_| CliError::new(ErrorKind::Input, format!("cannot read {label} file")))?;
    let effective_uid = rustix::process::geteuid().as_raw();
    if !opened.is_file()
        || opened.permissions().mode() & 0o077 != 0
        || !matches!(opened.uid(), 0) && opened.uid() != effective_uid
        || opened.len() == 0
        || opened.len() > maximum as u64
        || path_metadata.file_type().is_symlink()
        || path_metadata.dev() != opened.dev()
        || path_metadata.ino() != opened.ino()
    {
        return Err(CliError::new(
            ErrorKind::Input,
            format!(
                "{label} file must be private (0600 or stricter), regular, non-empty, and not a symlink"
            ),
        ));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(maximum.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| CliError::new(ErrorKind::Input, format!("cannot read {label} file")))?;
    let after = file
        .metadata()
        .map_err(|_| CliError::new(ErrorKind::Input, format!("cannot read {label} file")))?;
    if bytes.is_empty() || bytes.len() > maximum || bytes.len() as u64 != opened.len() {
        bytes.zeroize();
        return Err(CliError::new(
            ErrorKind::Input,
            format!("{label} file has an invalid size"),
        ));
    }
    if metadata_changed(&opened, &after) {
        bytes.zeroize();
        return Err(CliError::new(
            ErrorKind::Input,
            format!("{label} file changed while it was being read"),
        ));
    }
    String::from_utf8(bytes).map_err(|error| {
        let mut bytes = error.into_bytes();
        bytes.zeroize();
        CliError::new(ErrorKind::Input, format!("{label} file must contain UTF-8"))
    })
}

fn read_bounded_file(path: &Path, maximum: u64, label: &str) -> Result<Vec<u8>> {
    let descriptor = rustix::fs::openat2(
        rustix::fs::CWD,
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
        rustix::fs::ResolveFlags::NO_SYMLINKS | rustix::fs::ResolveFlags::NO_MAGICLINKS,
    )
    .map_err(|_| CliError::new(ErrorKind::Input, format!("cannot read {label}")))?;
    let mut file = fs::File::from(descriptor);
    let metadata = file
        .metadata()
        .map_err(|_| CliError::new(ErrorKind::Input, format!("cannot read {label}")))?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > maximum {
        return Err(CliError::new(
            ErrorKind::Input,
            format!("{label} must be a non-empty regular file no larger than {maximum} bytes"),
        ));
    }
    let capacity = usize::try_from(metadata.len())
        .map_err(|_| CliError::new(ErrorKind::Input, format!("{label} is too large")))?;
    let mut bytes = Vec::with_capacity(capacity);
    Read::by_ref(&mut file)
        .take(maximum.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| CliError::new(ErrorKind::Input, format!("cannot read {label}")))?;
    let after = file
        .metadata()
        .map_err(|_| CliError::new(ErrorKind::Input, format!("cannot read {label}")))?;
    if bytes.len() as u64 != metadata.len() || metadata_changed(&metadata, &after) {
        return Err(CliError::new(
            ErrorKind::Input,
            format!("{label} changed while it was being read"),
        ));
    }
    Ok(bytes)
}

fn metadata_changed(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
}

fn write_binary_output(path: Option<&Path>, bytes: &[u8], force: bool) -> Result<Option<PathBuf>> {
    let Some(path) = path else {
        io::stdout().write_all(bytes).map_err(|error| {
            CliError::new(
                ErrorKind::General,
                format!("could not write archive: {error}"),
            )
        })?;
        return Ok(None);
    };
    if path == Path::new("-") {
        io::stdout().write_all(bytes).map_err(|error| {
            CliError::new(
                ErrorKind::General,
                format!("could not write archive: {error}"),
            )
        })?;
        return Ok(None);
    }
    write_file(path, bytes, force, true)?;
    Ok(Some(path.to_owned()))
}

fn write_text_output(path: Option<&Path>, text: &str, force: bool) -> Result<Option<PathBuf>> {
    let Some(path) = path else {
        print!("{text}");
        return Ok(None);
    };
    if path == Path::new("-") {
        print!("{text}");
        return Ok(None);
    }
    write_file(path, text.as_bytes(), force, false)?;
    Ok(Some(path.to_owned()))
}

fn write_file(path: &Path, bytes: &[u8], force: bool, private: bool) -> Result<()> {
    write_output_file(path, bytes, force, private)
}

fn write_output_file(path: &Path, bytes: &[u8], force: bool, private: bool) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| CliError::new(ErrorKind::Input, "output destination must name a file"))?;
    let mut temporary = None;
    for nonce in 0..100_u32 {
        let candidate = parent.join(format!(
            ".{}.piquelctl-{}-{nonce}.tmp",
            name.to_string_lossy(),
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .mode(if private { 0o600 } else { 0o644 });
        match options.open(&candidate) {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(_) => {
                return Err(CliError::new(
                    ErrorKind::Refused,
                    "destination directory cannot be written",
                ));
            }
        }
    }
    let (temporary_path, mut file) = temporary.ok_or_else(|| {
        CliError::new(
            ErrorKind::General,
            "could not allocate a temporary output file",
        )
    })?;
    let write_result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary_path);
        return Err(CliError::new(ErrorKind::General, "local output I/O failed"));
    }
    let rename = if force {
        fs::rename(&temporary_path, path)
    } else {
        rustix::fs::renameat_with(
            rustix::fs::CWD,
            &temporary_path,
            rustix::fs::CWD,
            path,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(io::Error::from)
    };
    if rename.is_err() {
        let _ = fs::remove_file(&temporary_path);
        return Err(CliError::new(
            ErrorKind::Refused,
            "destination exists or cannot be replaced; use --force to replace it",
        ));
    }
    if let Ok(directory) = fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn digest(value: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(value))
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

async fn watch_operation(
    cli: &Cli,
    client: &Client,
    operation_id: &str,
    poll_seconds: u64,
) -> Result<OperationView> {
    let mut last_event_id = None;
    let mut seen_event_ids = BTreeSet::new();
    let mut event_order = std::collections::VecDeque::new();
    let mut stream_failures = 0_u8;
    let mut build_cursors = std::collections::BTreeMap::new();
    loop {
        let current = client.operation(operation_id).await?;
        report_operation_progress(cli, &current);
        report_build_progress(cli, client, operation_id, &mut build_cursors).await?;
        if terminal_operation(&current.state) {
            return finish_operation(current);
        }

        if stream_failures < 2 {
            let mut events = client.watch_operation(operation_id, last_event_id.as_deref());
            loop {
                tokio::select! {
                    signal = signal::ctrl_c() => {
                        signal.map_err(|_| CliError::new(ErrorKind::General, "could not install Ctrl-C handler"))?;
                        return Err(CliError::new(
                            ErrorKind::Interrupted,
                            "watch interrupted; the server-side operation was not cancelled",
                        ));
                    }
                    event = events.recv() => match event {
                        Some(Ok(event)) => {
                            if event.id.as_ref().is_some_and(|id| is_duplicate_event(&mut seen_event_ids, &mut event_order, id)) {
                                continue;
                            }
                            if let Some(id) = event.id {
                                last_event_id = Some(id);
                            }
                            if !cli.quiet {
                                eprintln!("operation event {}", event.event.as_deref().unwrap_or("message"));
                            }
                            if let Ok(operation) = serde_json::from_str::<OperationView>(&event.data) {
                                report_operation_progress(cli, &operation);
                                report_build_progress(cli, client, operation_id, &mut build_cursors).await?;
                                if terminal_operation(&operation.state) {
                                    return finish_operation(operation);
                                }
                            }
                        }
                        Some(Err(ClientError::Transport { .. })) | None => {
                            stream_failures = stream_failures.saturating_add(1);
                            break;
                        }
                        Some(Err(error)) => return Err(error.into()),
                    }
                }
            }
        } else {
            tokio::select! {
                signal = signal::ctrl_c() => {
                    signal.map_err(|_| CliError::new(ErrorKind::General, "could not install Ctrl-C handler"))?;
                    return Err(CliError::new(
                        ErrorKind::Interrupted,
                        "watch interrupted; the server-side operation was not cancelled",
                    ));
                }
                () = time::sleep(Duration::from_secs(poll_seconds.max(1))) => {}
            }
        }
    }
}

async fn report_build_progress(
    cli: &Cli,
    client: &Client,
    operation_id: &str,
    cursors: &mut std::collections::BTreeMap<String, u64>,
) -> Result<()> {
    if cli.quiet {
        return Ok(());
    }
    let builds = client.operation_builds(operation_id).await?.items;
    for build in builds {
        eprintln!(
            "  build {} ({}): {}",
            build.service_name, build.id, build.state
        );
        let cursor = cursors.entry(build.id.clone()).or_default();
        loop {
            let logs = client.build_logs(&build.id, *cursor, 100).await?.items;
            if logs.is_empty() {
                break;
            }
            for log in &logs {
                eprintln!("    {}", log.message);
                *cursor = (*cursor).max(log.sequence);
            }
            if logs.len() < 100 {
                break;
            }
        }
    }
    Ok(())
}

async fn follow_logs(
    cli: &Cli,
    client: &Client,
    id: &str,
    options: &ApplicationLogsOptions,
) -> Result<()> {
    let mut cursor = None;
    let mut seen_event_ids = BTreeSet::new();
    let mut event_order = std::collections::VecDeque::new();
    loop {
        let mut events = client.follow_application_logs(id, options, cursor.as_deref());
        loop {
            tokio::select! {
                signal = signal::ctrl_c() => {
                    signal.map_err(|_| CliError::new(ErrorKind::General, "could not install Ctrl-C handler"))?;
                    return Ok(());
                }
                event = events.recv() => match event {
                    Some(Ok(event)) => {
                        if event.id.as_ref().is_some_and(|id| is_duplicate_event(&mut seen_event_ids, &mut event_order, id)) {
                            continue;
                        }
                        if let Some(id) = event.id {
                            cursor = Some(id);
                        }
                        if cli.structured_json() {
                            emit_json_envelope("application_log_event", &json!({
                                "event": event.event,
                                "data": event.data,
                                "cursor": cursor,
                            }))?;
                        } else if cli.json {
                            emit_json(&json!({"event": event.event, "data": event.data}))?;
                        } else if !cli.quiet {
                            if let Ok(logs) = serde_json::from_str::<Vec<piqueld_client::ContainerLogView>>(&event.data) {
                                for log in logs {
                                    println!("{} {} {}: {}", log.timestamp, log.service, log.stream, log.display_message);
                                }
                            } else {
                                println!("{}", event.data);
                            }
                        }
                    }
                    Some(Err(ClientError::Transport { .. })) | None => {
                        if !cli.quiet {
                            eprintln!("log stream interrupted; reconnecting");
                        }
                        break;
                    }
                    Some(Err(error)) => return Err(error.into()),
                }
            }
        }
        time::sleep(Duration::from_millis(500)).await;
    }
}

fn report_operation_progress(cli: &Cli, operation: &OperationView) {
    if !cli.quiet {
        report_operation(operation);
    }
}

fn is_duplicate_event(
    seen: &mut BTreeSet<String>,
    order: &mut std::collections::VecDeque<String>,
    id: &str,
) -> bool {
    const WINDOW: usize = 1024;
    if !seen.insert(id.to_owned()) {
        return true;
    }
    order.push_back(id.to_owned());
    if order.len() > WINDOW
        && let Some(expired) = order.pop_front()
    {
        seen.remove(&expired);
    }
    false
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
    if cli.structured_json() {
        emit_json_envelope("operation", operation)
    } else if cli.json {
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

fn confirm_with_mode(cli: &Cli, yes: bool, prompt: &str) -> Result<()> {
    confirm_inner(cli.noninteractive, yes, prompt)
}

fn confirm_inner(noninteractive: bool, yes: bool, prompt: &str) -> Result<()> {
    if yes {
        return Ok(());
    }
    if noninteractive || !io::stdin().is_terminal() {
        return Err(CliError::new(
            ErrorKind::Refused,
            "confirmation is required; pass --yes",
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
            ErrorKind::Refused,
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
    let metadata = async_fs::metadata(path).await.map_err(|error| {
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
    let bytes = async_fs::read(path).await.map_err(|error| {
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

fn emit_json_envelope<T: Serialize>(kind: &str, data: &T) -> Result<()> {
    let value = json!({
        "schema": "piquelctl.v1",
        "kind": kind,
        "data": data,
    });
    emit_json(&value)
}

fn finish_error(cli: &Cli, error: CliError) -> ExitCode {
    if cli.structured_json() {
        let category = match error.kind {
            ErrorKind::Authentication => "authentication",
            ErrorKind::Conflict => "conflict",
            ErrorKind::Unavailable => "unavailable",
            ErrorKind::Operation => "operation_failed",
            ErrorKind::Interrupted => "interrupted",
            ErrorKind::Refused => "refused",
            ErrorKind::Input => "validation",
            ErrorKind::General => "general",
        };
        let value = json!({
            "schema": "piquelctl.error.v1",
            "error": {
                "category": category,
                "message": &error.message,
                "api_code": &error.api_code,
                "request_id": &error.request_id,
                "exit_code": error.kind.exit_code(),
            }
        });
        eprintln!(
            "{}",
            serde_json::to_string(&value)
                .unwrap_or_else(|_| { "{\"schema\":\"piquelctl.error.v1\"}".to_owned() })
        );
    } else if cli.json {
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
    ExitCode::from(if cli.structured_json() {
        error.kind.exit_code()
    } else {
        error.kind.legacy_exit_code()
    })
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
            vec![
                "piquelctl",
                "export",
                "--application",
                "notes",
                "--output",
                "application.toml",
                "--include-resolved",
            ],
            vec![
                "piquelctl",
                "export",
                "--output",
                "state.tar",
                "--mode",
                "encrypted",
                "--force",
            ],
            vec!["piquelctl", "import", "state.tar", "--yes"],
        ];
        for arguments in cases {
            Cli::try_parse_from(arguments).expect("command parses");
        }
    }

    #[test]
    fn parser_covers_the_advanced_grouped_surface() {
        let cases = [
            vec!["piquelctl", "--output", "json", "application", "list"],
            vec!["piquelctl", "application", "logs", "notes", "--follow"],
            vec!["piquelctl", "secret", "list"],
            vec!["piquelctl", "secret", "set", "database", "--file", "secret"],
            vec!["piquelctl", "operation", "watch", "operation-01"],
            vec![
                "piquelctl",
                "state",
                "export",
                "--file",
                "state.tar",
                "--mode",
                "encrypted",
            ],
            vec![
                "piquelctl",
                "state",
                "import",
                "state.tar",
                "--replace",
                "--yes",
            ],
        ];
        for arguments in cases {
            Cli::try_parse_from(arguments).expect("advanced command parses");
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

    #[test]
    fn advanced_exit_categories_are_stable() {
        assert_eq!(ErrorKind::Input.exit_code(), 3);
        assert_eq!(ErrorKind::Authentication.exit_code(), 4);
        assert_eq!(ErrorKind::Conflict.exit_code(), 5);
        assert_eq!(ErrorKind::Unavailable.exit_code(), 6);
        assert_eq!(ErrorKind::Operation.exit_code(), 7);
        assert_eq!(ErrorKind::Interrupted.exit_code(), 8);
        assert_eq!(ErrorKind::Refused.exit_code(), 9);
    }

    #[test]
    fn protected_inputs_and_atomic_outputs_are_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let token = directory.path().join("token");
        fs::write(&token, "canary\n").unwrap();
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_private_text(&token, 100, "token").unwrap(), "canary\n");
        fs::set_permissions(&token, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(read_private_text(&token, 100, "token").is_err());

        let output = directory.path().join("state.tar");
        write_output_file(&output, b"archive", false, true).unwrap();
        assert_eq!(
            fs::metadata(&output).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(write_output_file(&output, b"other", false, true).is_err());
        assert_eq!(fs::read(&output).unwrap(), b"archive");
    }

    #[test]
    fn event_deduplication_has_a_bounded_window() {
        let mut seen = BTreeSet::new();
        let mut order = std::collections::VecDeque::new();
        assert!(!is_duplicate_event(&mut seen, &mut order, "first"));
        assert!(is_duplicate_event(&mut seen, &mut order, "first"));
        for index in 0..1024 {
            assert!(!is_duplicate_event(
                &mut seen,
                &mut order,
                &format!("event-{index}")
            ));
        }
        assert_eq!(seen.len(), 1024);
        assert!(!seen.contains("first"));
    }
}
