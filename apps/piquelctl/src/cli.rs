//! Clap command-line surface. `///` on derived items is user-facing help text, so
//! internal notes use `//`. Flattened `Args` structs keep `//` because their doc
//! comments would become the `about` text of commands that flatten them.
use clap::{Args, Parser, Subcommand};
use std::{path::PathBuf, time::Duration};

/// Essential commands for inspecting and operating applications and their environments.
#[derive(Debug, Parser)]
#[command(
    name = "piquelctl",
    version,
    about = "Operate a local piqueld control plane"
)]
pub(crate) struct Cli {
    // Where the effective endpoint and timeout came from, filled in by
    // `Profiles::resolve` and shown in connection diagnostics.
    #[arg(skip)]
    pub(crate) connection_sources: crate::profiles::ConnectionSources,

    /// Named connection in the profiles file (or `PIQUELD_PROFILE`).
    #[arg(long, global = true)]
    pub(crate) profile: Option<String>,
    /// Profiles file (or `PIQUELD_PROFILES_FILE`); replaces system and user discovery.
    #[arg(long, global = true)]
    pub(crate) profiles_file: Option<PathBuf>,

    /// Unix socket path. The default is the daemon's local socket.
    #[arg(long, global = true, value_name = "PATH", conflicts_with = "url")]
    pub(crate) socket: Option<PathBuf>,

    #[command(flatten)]
    pub(crate) auth: AuthArgs,

    /// Explicit HTTP or HTTPS endpoint.
    #[arg(long, global = true, value_name = "URL", conflicts_with = "socket")]
    pub(crate) url: Option<String>,

    /// Bound for each request and for the complete command wait.
    #[arg(long, global = true, default_value = "30s", value_parser = parse_duration)]
    pub(crate) timeout: Duration,

    /// Emit only the command's documented JSON result on stdout.
    #[arg(long, global = true)]
    pub(crate) json: bool,

    /// Suppress human results, info and progress; JSON, warnings and errors remain available.
    #[arg(long, short, global = true)]
    pub(crate) quiet: bool,
    /// Never prompt, even on a terminal. Mutations still require --yes.
    #[arg(long, global = true)]
    pub(crate) noninteractive: bool,

    #[command(subcommand)]
    pub(crate) command: Command,
}

// Account selection and authentication transport options.
#[derive(Debug, Args)]
pub(crate) struct AuthArgs {
    /// Account username or ID from the private credential file.
    #[arg(long, global = true)]
    pub(crate) account: Option<String>,

    /// Allow remote HTTP authentication (only with separate transport encryption, e.g. Tailscale).
    #[arg(long, global = true)]
    pub(crate) allow_insecure_http: bool,
}

// Top-level commands. `Profiles` and `Login` are dispatched by `main` before the
// timeout-bounded `commands::run`; everything else goes through it.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Sign in with a passkey through the browser (also works over SSH).
    /// Grant options limit the session; `--app` takes application IDs here.
    Login {
        #[command(flatten)]
        limit: crate::accounts::GrantArgs,
    },
    /// Revoke the current credential and remove its local copy.
    Logout,
    /// Show the authenticated account and what the current credential may do.
    Whoami,
    /// List accounts, change their grants, and create invitation or enrollment links.
    Account {
        #[command(subcommand)]
        command: crate::accounts::AccountCommand,
    },
    /// Create, list, and revoke API tokens for your account.
    Token {
        #[command(subcommand)]
        command: crate::accounts::TokenCommand,
    },
    /// Read the audit trail of API requests, newest first: your own, or any
    /// account's with audit:read.
    Audit(crate::accounts::AuditArgs),
    /// Print a one-time link that creates a new administrator account, to
    /// regain access when no administrator can sign in. Run it with sudo on
    /// the daemon host: only root or the daemon's user may use it (Unix
    /// socket only).
    RecoverAdmin,
    /// Print a one-time link that signs a browser in as the host operator,
    /// with full access, for 12 hours. Run it on the daemon host as root or
    /// the daemon's user (Unix socket only); the link works once within 10
    /// minutes.
    SignInLink,
    /// Print the first-account setup link (Unix socket only).
    SetupLink {
        /// Also open the link in the default browser.
        #[arg(long)]
        open: bool,
    },
    /// List effective connection profile names and endpoints without contacting a daemon.
    Profiles,
    /// Manage the encryption key for all secrets on this daemon.
    Secrets {
        #[command(subcommand)]
        action: crate::secrets::KeyAction,
    },
    /// Report daemon availability and version.
    Status,
    /// Inspect the daemon's DNS providers.
    Dns {
        #[command(subcommand)]
        command: DnsCommand,
    },
    /// Create, inspect, edit, and deploy applications.
    App {
        #[command(subcommand)]
        command: AppCommand,
    },
    /// Create, inspect, deploy, and delete an application's environments.
    Env {
        #[command(subcommand)]
        command: crate::environments::EnvCommand,
    },
    /// Inspect build attempts and their persisted output.
    Builds(BuildArgs),
    /// Inspect or wait for one asynchronous operation.
    Operation(OperationArgs),
    /// Read one page of informational events, oldest first.
    Events {
        /// Filter by stable application ID: its own events and those of all its
        /// environments, including deleted ones.
        #[arg(long)]
        application: Option<String>,
        /// Filter by stable environment ID, including deleted environments.
        #[arg(long)]
        environment: Option<String>,
        /// Continue after a cursor returned by the previous page.
        #[arg(long)]
        cursor: Option<String>,
        /// Maximum number of events in the page.
        #[arg(long,default_value_t=50,value_parser=clap::value_parser!(u16).range(1..=100))]
        limit: u16,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum DnsCommand {
    /// Check every DNS provider's credentials and zones now instead of at the
    /// next hourly discovery. Credential files are only read at daemon startup.
    Refresh,
}

#[derive(Debug, Subcommand)]
pub(crate) enum AppCommand {
    /// Save an empty application; add services and volumes independently.
    Create(CreateArgs),
    /// Edit individual service settings.
    Service {
        #[command(subcommand)]
        command: crate::editing::ServiceCommand,
    },
    /// Add or remove declared named volumes.
    Volume {
        #[command(subcommand)]
        command: crate::editing::VolumeCommand,
    },
    /// Add or remove public HTTPS routes.
    Route {
        #[command(subcommand)]
        command: crate::editing::RouteCommand,
    },
    /// Add, replace, or remove one-shot jobs that run before rollout.
    Job {
        #[command(subcommand)]
        command: crate::editing::JobCommand,
    },
    /// Set or remove manifest variables and their values per environment.
    Variable {
        #[command(subcommand)]
        command: crate::editing::VariableCommand,
    },
    /// Connect, edit, or disconnect the manifest repository.
    Repository {
        #[command(subcommand)]
        command: crate::editing::RepositoryCommand,
    },
    /// List applications, their environments, and concise reconciliation status.
    List,
    /// Show one application and its environments by name or ID.
    Show {
        /// Application name or stable ID.
        name_or_id: String,
    },
    /// Read a bounded snapshot of Docker logs of the application's only environment.
    Logs {
        /// Application name or stable ID.
        name_or_id: String,
        #[command(flatten)]
        window: LogArgs,
    },
    /// Run a one-off command in a running task of a service, streaming its output.
    Exec(crate::exec::ExecArgs),
    /// Manage the application's store of manually set secrets and their access lists.
    Secret {
        /// Application name or stable ID.
        application: String,
        #[command(subcommand)]
        action: crate::secrets::SecretAction,
    },
    /// Check a TOML manifest locally, without contacting a daemon.
    Validate {
        /// TOML application manifest.
        #[arg(long, value_name = "PATH")]
        file: PathBuf,
    },
    /// Preview creation or replacement from a TOML manifest.
    Plan(ManifestArgs),
    /// Save a TOML manifest; optionally deploy with --deploy.
    Apply(ApplyArgs),
    /// Confirm and delete an application and all its environments by name or ID.
    Delete(DeleteArgs),
    /// Retry the latest operation of the application's only environment once it
    /// has ended or failed, reusing its saved inputs.
    Reconcile(TargetArgs),
    /// Deploy saved configuration to the application's only environment, fetching
    /// the manifest and resolving sources again.
    Deploy(DeployArgs),
    /// Rename an idle application; optionally deploy with the change.
    Rename(RenameArgs),
    /// Export the saved manifest as TOML.
    Manifest {
        /// Application name or stable ID.
        name_or_id: String,
    },
}

#[derive(Debug, Args)]
pub(crate) struct CreateArgs {
    /// New unique application name.
    pub(crate) name: String,
    #[command(flatten)]
    pub(crate) deployment: DeploymentArgs,
    /// Skip interactive confirmation.
    #[arg(long)]
    pub(crate) yes: bool,
}

#[derive(Debug, Args)]
pub(crate) struct BuildArgs {
    #[command(subcommand)]
    pub(crate) command: BuildCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum BuildCommand {
    /// List recent build attempts, newest first.
    List {
        /// Filter by stable application ID: builds of all its environments.
        #[arg(long)]
        application: Option<String>,
        /// Filter by stable environment ID.
        #[arg(long)]
        environment: Option<String>,
        /// Continue after a cursor returned by the previous page.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Read a bounded page of persisted build output.
    Logs {
        /// Build attempt ID from `builds list`.
        id: i64,
        /// Read output before this offset, as suggested after the previous page.
        #[arg(long)]
        before: Option<i64>,
    },
}

#[derive(Debug, Args)]
pub(crate) struct ManifestArgs {
    /// TOML application manifest.
    #[arg(long, value_name = "PATH")]
    pub(crate) file: PathBuf,
    /// Require this intent generation; zero requires an absent application.
    #[arg(long)]
    pub(crate) expected_generation: Option<u64>,
    /// Compare with this environment's deployment and runtime; defaults to the
    /// application's only environment.
    #[arg(long = "env", value_name = "ENV")]
    pub(crate) environment: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct ApplyArgs {
    #[command(flatten)]
    pub(crate) deployment: DeploymentArgs,
    /// TOML application manifest.
    #[arg(long, value_name = "PATH")]
    pub(crate) file: PathBuf,
    /// Require this intent generation; zero requires an absent application.
    #[arg(long)]
    pub(crate) expected_generation: Option<u64>,

    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub(crate) yes: bool,

    /// Override intent preconditions (does not skip confirmation).
    #[arg(long, conflicts_with = "expected_generation")]
    pub(crate) force: bool,
}

// Shared `--deploy` / `--no-wait` flags for commands that save configuration.
#[derive(Debug, Args)]
pub(crate) struct DeploymentArgs {
    /// Deploy after saving; by default only configuration is saved.
    #[arg(long)]
    pub(crate) deploy: bool,
    /// Return immediately after saving or accepting deployment.
    #[arg(long)]
    pub(crate) no_wait: bool,
}

#[derive(Debug, Args)]
pub(crate) struct DeleteArgs {
    /// Application name or stable ID.
    pub(crate) name_or_id: String,
    /// Names of every environment, required when the application has several.
    #[arg(long, value_delimiter = ',', value_name = "NAMES")]
    pub(crate) environments: Vec<String>,
    #[command(flatten)]
    pub(crate) deletion: DeletionFlags,
}

// Precondition, confirmation, and waiting flags shared by deletions.
#[derive(Debug, Args)]
pub(crate) struct DeletionFlags {
    /// Require this application revision.
    #[arg(long)]
    pub(crate) expected_generation: Option<u64>,

    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub(crate) yes: bool,

    /// Override revision preconditions (does not skip confirmation).
    #[arg(long, conflicts_with = "expected_generation")]
    pub(crate) force: bool,

    /// Return after the daemon accepts the operation.
    #[arg(long)]
    pub(crate) no_wait: bool,
}

#[derive(Debug, Args)]
pub(crate) struct OperationArgs {
    /// Stable operation ID.
    pub(crate) operation_id: String,

    /// Fetch once instead of waiting for a terminal state.
    #[arg(long)]
    pub(crate) no_wait: bool,
}

/// Parses a positive integer duration with an optional unit (`ms`, `s`, `m`, `h`;
/// bare numbers are seconds). Used for `--timeout`, `PIQUELD_TIMEOUT`, and profiles.
///
/// ```text
/// "500ms" → 500ms    "30" → 30s    "2m" → 120s    "0s" / "1.5s" / "-1" → error
/// ```
pub(crate) fn parse_duration(value: &str) -> std::result::Result<Duration, String> {
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

#[derive(Debug, Args)]
pub(crate) struct RenameArgs {
    /// Existing application name or stable ID.
    pub(crate) name_or_id: String,
    /// New unique application name.
    pub(crate) new_name: String,
    #[command(flatten)]
    pub(crate) edit: crate::editing::EditFlags,
}

#[derive(Debug, Args)]
pub(crate) struct DeployArgs {
    #[command(flatten)]
    pub(crate) target: TargetArgs,
    #[command(flatten)]
    pub(crate) revision: RevisionArgs,
}

// One-time manifest revision overrides for deployments.
#[derive(Debug, Args)]
pub(crate) struct RevisionArgs {
    /// Fetch the repository manifest from this branch instead of the
    /// environment's, for this deployment only.
    #[arg(long, conflicts_with = "commit")]
    pub(crate) branch: Option<String>,
    /// Fetch the repository manifest from this full commit instead, for this
    /// deployment only.
    #[arg(long)]
    pub(crate) commit: Option<String>,
}

impl RevisionArgs {
    /// The one-time manifest revision, if overridden.
    pub(crate) fn revision(&self) -> Option<piqueld_client::ManifestRevision> {
        self.branch
            .clone()
            .map(piqueld_client::ManifestRevision::Branch)
            .or_else(|| {
                self.commit
                    .clone()
                    .map(piqueld_client::ManifestRevision::Commit)
            })
    }
}

/// Application selection, confirmation, and waiting shared by `app reconcile` and `app deploy`.
#[derive(Debug, Args)]
pub(crate) struct TargetArgs {
    /// Application name or stable ID.
    pub(crate) name_or_id: String,
    #[command(flatten)]
    pub(crate) flags: OperationFlags,
}

// Precondition, confirmation, and waiting flags shared by deploy and reconcile.
#[derive(Debug, Args)]
pub(crate) struct OperationFlags {
    /// Optionally require this application revision.
    #[arg(long)]
    pub(crate) expected_generation: Option<u64>,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Return after the daemon accepts the operation.
    #[arg(long)]
    pub(crate) no_wait: bool,
}

// Log window shared by `app logs` and `env logs`.
#[derive(Debug, Args)]
pub(crate) struct LogArgs {
    /// Only include output from this service; all services when omitted.
    #[arg(long)]
    pub(crate) service: Option<String>,
    /// Maximum number of most recent lines, merged across the selected services.
    #[arg(long,default_value_t=200,value_parser=clap::value_parser!(u16).range(1..=1000))]
    pub(crate) tail: u16,
    /// Only include output from the last N seconds.
    #[arg(long,default_value_t=3600,value_parser=clap::value_parser!(u32).range(1..=86400))]
    pub(crate) since_seconds: u32,
}
