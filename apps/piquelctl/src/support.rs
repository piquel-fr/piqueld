use crate::{
    cli::Cli,
    error::{CliError, ErrorKind, Result},
};
use piqueld_client::{ApplicationView, ClientError, ValidationErrors};
use serde_json::json;
use std::{
    future::Future,
    io::{self, IsTerminal, Write},
    path::Path,
    time::Duration,
};
use tokio::{fs, time};
use uuid::Uuid;

pub(crate) const DEFAULT_SOCKET: &str = "/run/piqueld/piqueld.sock";
pub(crate) const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
pub(crate) const MAX_PAGINATION_PAGES: usize = 10_000;
pub(crate) const PAGE_SIZE: u16 = 100;
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(250);

pub(crate) fn confirm(yes: bool, prompt: &str) -> Result<()> {
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

pub(crate) async fn read_manifest(path: &Path) -> Result<String> {
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

pub(crate) fn manifest_name(manifest: &str, path: &Path) -> Result<String> {
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

pub(crate) fn idempotency_key() -> String {
    format!("piquelctl-{}", Uuid::now_v7().simple())
}

pub(crate) fn transport_description(cli: &Cli) -> String {
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

pub(crate) fn desired_replicas(application: &ApplicationView) -> u32 {
    application
        .application
        .spec
        .services
        .iter()
        .map(|service| u32::from(service.replicas))
        .sum()
}

pub(crate) fn looks_like_application_id(value: &str) -> bool {
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

pub(crate) fn format_duration(duration: Duration) -> String {
    if duration.as_millis().is_multiple_of(1000) {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

pub(crate) fn terminal_operation(state: &str) -> bool {
    matches!(
        state.to_ascii_lowercase().as_str(),
        "succeeded" | "completed" | "failed" | "cancelled" | "canceled"
    )
}
