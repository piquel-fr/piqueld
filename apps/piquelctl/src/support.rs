//! Shared helpers: confirmation prompts, blocking input, manifest reading, retries,
//! and small formatting utilities.
use crate::{
    cli::Cli,
    error::{CliError, ErrorKind, Result},
    output::Console,
};
use piqueld_client::{ClientError, ValidationErrors};
use serde_json::json;
use std::{
    future::Future,
    io::{self, IsTerminal, Read},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tokio::{sync::Notify, time};

/// Daemon socket used when no socket or URL is configured.
pub(crate) const DEFAULT_SOCKET: &str = "/run/piqueld/piqueld.sock";
/// Must not exceed the daemon's `REQUEST_BODY_LIMIT_BYTES`, or a locally
/// accepted manifest would fail server-side with 413.
pub(crate) const MAX_MANIFEST_BYTES: u64 = 2 * 1024 * 1024;
/// Page size for exhaustive application listing (the API maximum).
pub(crate) const PAGE_SIZE: u16 = piqueld_client::MAX_APPLICATION_PAGE_SIZE;
/// Delay between operation status polls.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Set while a blocking operator read is in progress; see `read_input`.
static INTERACTION_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Wakes the timeout supervisor in `main` when `INTERACTION_ACTIVE` flips.
static INTERACTION_CHANGED: Notify = Notify::const_new();

/// Updates the interaction flag and wakes the supervisor.
fn set_interaction(active: bool) {
    INTERACTION_ACTIVE.store(active, Ordering::SeqCst);
    // `notify_one` stores a permit when no waiter is registered yet, so a
    // transition can never be lost by the timeout supervisor.
    INTERACTION_CHANGED.notify_one();
}

/// Returns whether an interactive prompt is currently waiting for operator
/// input; the surrounding command timeout must not consume budget meanwhile.
pub(crate) fn interaction_active() -> bool {
    INTERACTION_ACTIVE.load(Ordering::SeqCst)
}

/// Resolves whenever the interaction flag changes.
pub(crate) async fn interaction_changed() {
    INTERACTION_CHANGED.notified().await;
}

/// Asks for `y`/`yes` on stdin before a mutation. `--yes` skips the prompt; without it,
/// `--noninteractive` or a non-terminal stdin is an input error. Any other answer
/// fails with "operation was not confirmed".
pub(crate) async fn confirm(
    console: &mut Console,
    noninteractive: bool,
    yes: bool,
    prompt: &str,
) -> Result<()> {
    if yes {
        return Ok(());
    }
    if noninteractive || !io::stdin().is_terminal() {
        return Err(CliError::new(
            ErrorKind::Input,
            "confirmation is required in a non-interactive terminal; pass --yes",
        ));
    }
    console.prompt(prompt)?;
    let answer = read_input("confirmation", || {
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        Ok(answer)
    })
    .await?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        Err(CliError::new(
            ErrorKind::Input,
            "operation was not confirmed",
        ))
    }
}

/// Runs a blocking read of operator input off the async runtime, so the
/// command supervisor in `main` keeps handling Ctrl-C. The command timeout is
/// suspended while the read is open; `main` exits the process directly if the
/// command fails mid-read, because the blocking reader cannot be cancelled.
pub(crate) async fn read_input<T: Send + 'static>(
    what: &str,
    read: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> Result<T> {
    set_interaction(true);
    let result = tokio::task::spawn_blocking(read).await;
    set_interaction(false);
    result.map_err(io::Error::other).flatten().map_err(|error| {
        CliError::new(
            ErrorKind::General,
            format!("could not read {what}: {error}"),
        )
    })
}

/// Reads a UTF-8 manifest of at most `MAX_MANIFEST_BYTES` on a blocking thread.
/// Only regular files are accepted; the type is checked both before and after
/// opening, and the read itself is capped in case the file grows.
pub(crate) async fn read_manifest(path: &Path) -> Result<String> {
    let path = path.to_owned();
    let display_path = path.display().to_string();
    tokio::task::spawn_blocking(move || {
        let metadata = std::fs::metadata(&path).map_err(|error| {
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
        let file = std::fs::File::open(&path).map_err(|error| {
            CliError::new(
                ErrorKind::Input,
                format!("could not read manifest {}: {error}", path.display()),
            )
        })?;
        let metadata = file.metadata().map_err(|error| {
            CliError::new(
                ErrorKind::Input,
                format!("could not inspect manifest {}: {error}", path.display()),
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
        // Cap the read itself so a concurrently growing file cannot force an
        // unbounded allocation after the metadata check.
        let mut bytes = Vec::new();
        file.take(MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| {
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
    })
    .await
    .map_err(|error| {
        CliError::new(
            ErrorKind::General,
            format!("could not read manifest {display_path}: {error}"),
        )
    })?
}

/// Extracts the application name from manifest TOML, reporting validation errors
/// against `path`.
pub(crate) fn manifest_name(manifest: &str, path: &Path) -> Result<String> {
    piqueld_client::application_name_from_toml(manifest)
        .map_err(|errors| manifest_validation_error(path, &errors))
}

/// Input error listing manifest validation failures, with structured details.
fn manifest_validation_error(path: &Path, errors: &ValidationErrors) -> CliError {
    CliError::new(
        ErrorKind::Input,
        format!(
            "manifest {} failed validation with {} error(s)",
            path.display(),
            errors.0.len()
        ),
    )
    .with_details(json!({"errors": errors}))
}

/// Runs `request`, retrying once after 25ms if it failed at the transport level
/// (e.g. a dropped connection). API errors and successes are returned as is.
pub(crate) async fn retry_transport<T, F, Fut>(
    mut request: F,
) -> std::result::Result<T, ClientError>
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

/// Human description of the configured endpoint.
///
/// ```text
/// --url http://127.0.0.1:7845  →  TCP http://127.0.0.1:7845
/// (no endpoint)                →  Unix socket /run/piqueld/piqueld.sock
/// ```
pub(crate) fn transport_description(cli: &Cli) -> String {
    if let Some(url) = &cli.url {
        format!("TCP {url}")
    } else {
        let socket = cli.socket.as_deref().map_or_else(
            || DEFAULT_SOCKET.to_owned(),
            |path| path.to_string_lossy().into_owned(),
        );
        format!("Unix socket {socket}")
    }
}

/// Whether `value` parses as an application ID (names may also match this shape).
pub(crate) fn looks_like_application_id(value: &str) -> bool {
    piqueld_client::ApplicationId::parse(value).is_ok()
}

/// Formats a duration in whole seconds when exact, else in milliseconds.
///
/// ```text
/// 30s → "30s"    1500ms → "1500ms"
/// ```
pub(crate) fn format_duration(duration: Duration) -> String {
    if duration.as_millis().is_multiple_of(1000) {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}
