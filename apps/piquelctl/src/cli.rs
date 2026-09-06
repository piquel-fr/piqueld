use clap::{Args, Parser, Subcommand};
use std::{path::PathBuf, time::Duration};

/// Essential commands for inspecting and operating Plan 06 applications.
#[derive(Debug, Parser)]
#[command(
    name = "piquelctl",
    version,
    about = "Operate a local piqueld control plane"
)]
pub(crate) struct Cli {
    /// Unix socket path. The default is the daemon's local socket.
    #[arg(long, global = true, value_name = "PATH", conflicts_with = "url")]
    pub(crate) socket: Option<PathBuf>,

    /// Explicit loopback HTTP endpoint, for example <http://127.0.0.1:8080/>.
    #[arg(long, global = true, value_name = "URL", conflicts_with = "socket")]
    pub(crate) url: Option<String>,

    /// Bound for each request and for the complete command wait.
    #[arg(long, global = true, default_value = "30s", value_parser = parse_duration)]
    pub(crate) timeout: Duration,

    /// Emit only the command's documented JSON result on stdout.
    #[arg(long, global = true)]
    pub(crate) json: bool,

    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
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
}

#[derive(Debug, Args)]
pub(crate) struct ManifestArgs {
    /// TOML application manifest.
    #[arg(long, value_name = "PATH")]
    pub(crate) file: PathBuf,

    /// Generation to require when replacing an existing application.
    #[arg(long, value_parser = parse_generation)]
    pub(crate) expected_generation: Option<u64>,
}

#[derive(Debug, Args)]
pub(crate) struct ApplyArgs {
    /// TOML application manifest.
    #[arg(long, value_name = "PATH")]
    pub(crate) file: PathBuf,

    /// Generation to require when replacing an existing application.
    #[arg(long, value_parser = parse_generation)]
    pub(crate) expected_generation: Option<u64>,

    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub(crate) yes: bool,

    /// Return after the daemon accepts the operation.
    #[arg(long)]
    pub(crate) no_wait: bool,
}

#[derive(Debug, Args)]
pub(crate) struct DeleteArgs {
    /// Application name or stable ID.
    pub(crate) name_or_id: String,

    /// Generation to require for deletion.
    #[arg(long, value_parser = parse_generation)]
    pub(crate) expected_generation: Option<u64>,

    /// Skip the interactive confirmation prompt.
    #[arg(long)]
    pub(crate) yes: bool,

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

pub(crate) fn parse_generation(value: &str) -> std::result::Result<u64, String> {
    let generation = value
        .parse::<u64>()
        .map_err(|_| "generation must be a positive integer".to_owned())?;
    (generation > 0)
        .then_some(generation)
        .ok_or_else(|| "generation must be a positive integer".to_owned())
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
