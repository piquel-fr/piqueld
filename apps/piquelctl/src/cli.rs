use clap::{Args, Parser, Subcommand};
use std::{path::PathBuf, time::Duration};

/// Essential commands for inspecting and operating applications.
#[derive(Debug, Parser)]
#[command(
    name = "piquelctl",
    version,
    about = "Operate a local piqueld control plane"
)]
pub(crate) struct Cli {
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

    /// Explicit HTTP endpoint, for example <http://127.0.0.1:8080/>.
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

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// List effective connection profile names and endpoints without contacting a daemon.
    Profiles,
    /// Manage the encryption key for all application secrets on this daemon.
    Secrets {
        #[command(subcommand)]
        action: crate::secrets::KeyAction,
    },
    /// Report daemon availability and version.
    Status,
    /// Create, inspect, edit, and deploy applications.
    App {
        #[command(subcommand)]
        command: AppCommand,
    },
    /// Inspect build attempts and their persisted output.
    Builds(BuildArgs),
    /// Inspect or wait for one asynchronous operation.
    Operation(OperationArgs),
    /// Read one page of informational events, oldest first.
    Events {
        /// Filter by stable application ID, including deleted applications.
        #[arg(long)]
        application: Option<String>,
        /// Continue after a cursor returned by the previous page.
        #[arg(long)]
        cursor: Option<String>,
        /// Maximum number of events in the page.
        #[arg(long,default_value_t=50,value_parser=clap::value_parser!(u16).range(1..=100))]
        limit: u16,
    },
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
    /// Connect, edit, or disconnect the manifest repository.
    Repository {
        #[command(subcommand)]
        command: crate::editing::RepositoryCommand,
    },
    /// List applications and their concise reconciliation status.
    List,
    /// Show one application by name or ID.
    Show {
        /// Application name or stable ID.
        name_or_id: String,
    },
    /// Read a bounded snapshot of Docker application logs.
    Logs {
        name_or_id: String,
        #[arg(long)]
        service: Option<String>,
        #[arg(long,default_value_t=200,value_parser=clap::value_parser!(u16).range(1..=1000))]
        tail: u16,
        #[arg(long,default_value_t=3600,value_parser=clap::value_parser!(u32).range(1..=86400))]
        since_seconds: u32,
    },
    /// Manage application-scoped secret values and metadata.
    Secret {
        application: String,
        #[command(subcommand)]
        action: crate::secrets::SecretAction,
    },
    /// Preview creation or replacement from a TOML manifest.
    Plan(ManifestArgs),
    /// Save a TOML manifest; optionally deploy with --deploy.
    Apply(ApplyArgs),
    /// Confirm and delete an application by name or ID.
    Delete(DeleteArgs),
    /// Repair latest intent without refreshing resolved images.
    Reconcile(ReconcileArgs),
    /// Deploy saved configuration with fresh source resolution.
    Deploy(ReconcileArgs),
    /// Rename an idle application; optionally deploy with the change.
    Rename(RenameArgs),
    /// Export the saved manifest as TOML.
    Manifest { name_or_id: String },
}

#[derive(Debug, Args)]
pub(crate) struct CreateArgs {
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
        #[arg(long)]
        application: Option<String>,
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Read a bounded page of persisted build output.
    Logs {
        id: i64,
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
    /// Require this intent generation.
    #[arg(long)]
    pub(crate) expected_generation: Option<u64>,

    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub(crate) yes: bool,

    /// Override intent preconditions (does not skip confirmation).
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
pub(crate) struct ReconcileArgs {
    /// Application name or stable ID; acts on its latest accepted intent.
    pub(crate) name_or_id: String,
    /// Optionally require this intent generation.
    #[arg(long)]
    pub(crate) expected_generation: Option<u64>,
    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub(crate) yes: bool,
    /// Return after the daemon accepts the operation.
    #[arg(long)]
    pub(crate) no_wait: bool,
}
