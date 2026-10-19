//! Shared helpers: blocking operator input, file reading, retries, and small
//! formatting utilities.
use crate::error::{CliError, ErrorKind, Result};
use piqueld_client::ClientError;
use serde_json::json;
use std::{
    fmt,
    future::Future,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tokio::{sync::Notify, time};

/// Daemon socket used when no socket or URL is configured.
pub(crate) const DEFAULT_SOCKET: &str = "/run/piqueld/piqueld.sock";
/// Page size for exhaustive application listing (the API maximum).
pub(crate) const PAGE_SIZE: u16 = piqueld_client::MAX_PAGE_SIZE;
/// Delay between operation status polls.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Delay between readiness polls once an operation ended; each one observes
/// the runtime, and the daemon checks routes far less often.
pub(crate) const SETTLE_INTERVAL: Duration = Duration::from_secs(1);

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

/// Reads a UTF-8 manifest no larger than the daemon accepts in a request
/// body; see [`read_text`].
pub(crate) async fn read_manifest(path: &Path) -> Result<String> {
    read_text(
        path,
        "manifest",
        piqueld_client::MAX_REQUEST_BODY_BYTES as u64,
    )
    .await
}

/// Reads the UTF-8 `what` at `path`, of at most `limit` bytes, on a blocking
/// thread. Only regular files are accepted; the type is checked both before
/// and after opening, and the read itself is capped in case the file grows.
pub(crate) async fn read_text(path: &Path, what: &'static str, limit: u64) -> Result<String> {
    let path = path.to_owned();
    let display_path = path.display().to_string();
    tokio::task::spawn_blocking(move || {
        let shown = path.display();
        let invalid =
            |problem: fmt::Arguments<'_>| CliError::new(ErrorKind::Input, problem.to_string());
        let unreadable =
            |error: io::Error| invalid(format_args!("could not read {what} {shown}: {error}"));
        let regular = |metadata: std::fs::Metadata| {
            if metadata.is_file() {
                Ok(metadata)
            } else {
                Err(invalid(format_args!(
                    "{what} path {shown} is not a regular file"
                )))
            }
        };
        regular(std::fs::metadata(&path).map_err(unreadable)?)?;
        let file = std::fs::File::open(&path).map_err(unreadable)?;
        let metadata = regular(file.metadata().map_err(|error| {
            invalid(format_args!("could not inspect {what} {shown}: {error}"))
        })?)?;
        if metadata.len() > limit {
            return Err(invalid(format_args!(
                "{what} {shown} exceeds the {limit}-byte input limit"
            )));
        }
        // Cap the read itself so a concurrently growing file cannot force an
        // unbounded allocation after the metadata check.
        let mut bytes = Vec::new();
        file.take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(unreadable)?;
        if bytes.len() as u64 > limit {
            return Err(invalid(format_args!(
                "{what} {shown} exceeds the input limit"
            )));
        }
        String::from_utf8(bytes)
            .map_err(|_| invalid(format_args!("{what} {shown} is not valid UTF-8")))
    })
    .await
    .map_err(|error| {
        CliError::new(
            ErrorKind::General,
            format!("could not read {what} {display_path}: {error}"),
        )
    })?
}

/// Extracts the application name from manifest TOML. Validation failures are
/// an input error against `path`, listing them in structured details.
pub(crate) fn manifest_name(manifest: &str, path: &Path) -> Result<String> {
    piqueld_client::application_name_from_toml(manifest).map_err(|errors| {
        CliError::new(
            ErrorKind::Input,
            format!(
                "manifest {} failed validation with {} error(s)",
                path.display(),
                errors.0.len()
            ),
        )
        .with_details(json!({"errors": errors}))
    })
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

/// The user's piqueld configuration directory, holding the profiles and
/// credentials files: `$XDG_CONFIG_HOME/piqueld`, else `$HOME/.config/piqueld`.
pub(crate) fn config_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|root| root.join("piqueld"))
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

/// `items` joined with `, `, or `empty` when there are none.
///
/// ```text
/// ["a", "b"] → "a, b"    [] → empty
/// ```
pub(crate) fn join_or<T: fmt::Display>(items: impl IntoIterator<Item = T>, empty: &str) -> String {
    let items = items
        .into_iter()
        .map(|item| item.to_string())
        .collect::<Vec<_>>();
    if items.is_empty() {
        empty.to_owned()
    } else {
        items.join(", ")
    }
}
