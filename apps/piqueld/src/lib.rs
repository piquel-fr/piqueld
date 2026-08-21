//! Daemon bootstrap and internal module boundaries.

pub mod api;
pub mod config;
pub mod docker;
pub mod operations;
pub mod reconcile;
pub mod store;

use std::{os::unix::fs::PermissionsExt, path::Path};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Prepares the Unix API socket's parent directory.
///
/// A missing directory is created with mode `0700`. An existing directory is
/// never modified but must be a real (non-symlink) directory that grants no
/// access to group or other users; anyone able to write there could replace
/// the daemon socket and intercept operator connections.
///
/// # Errors
///
/// Returns an [`std::io::Error`] when the directory cannot be inspected or
/// prepared, or when it violates the privacy requirements.
pub async fn prepare_socket_directory(parent: &Path) -> std::io::Result<()> {
    if parent == Path::new("/") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Unix API socket must be inside a private directory",
        ));
    }
    let existing = match tokio::fs::symlink_metadata(parent).await {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if let Some(metadata) = &existing {
        if metadata.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "Unix socket directory {} must not be a symlink",
                    parent.display()
                ),
            ));
        }
        let mode = metadata.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "Unix socket directory {} must be private (mode {:o} grants group or other access)",
                    parent.display(),
                    mode & 0o777,
                ),
            ));
        }
    }
    tokio::fs::create_dir_all(parent).await?;
    if existing.is_none() {
        tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await?;
    }
    Ok(())
}

/// Errors produced by the daemon's process-level runtime skeleton.
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// Installing a process signal handler failed.
    #[error("could not install the operating-system signal handler")]
    Signal(#[source] std::io::Error),
}

/// Waits until the supplied cancellation token is cancelled.
///
/// Long-running controller tasks receive child tokens rooted at this token.
///
/// # Errors
///
/// The wait itself is infallible; the result keeps process shutdown consistent
/// with the signal-handler boundary.
pub async fn run_until_cancelled(cancellation: CancellationToken) -> Result<(), RuntimeError> {
    info!("daemon started");
    cancellation.cancelled().await;
    info!("shutdown requested");
    Ok(())
}

/// Converts SIGINT or SIGTERM into cooperative daemon cancellation.
///
/// # Errors
///
/// Returns an error if the operating-system signal handler cannot be installed.
pub async fn cancel_on_shutdown_signal(
    cancellation: CancellationToken,
) -> Result<(), RuntimeError> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate = signal(SignalKind::terminate()).map_err(RuntimeError::Signal)?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.map_err(RuntimeError::Signal)?,
            _ = terminate.recv() => {},
            () = cancellation.cancelled() => return Ok(()),
        }
    }

    #[cfg(not(unix))]
    tokio::select! {
        result = tokio::signal::ctrl_c() => result.map_err(RuntimeError::Signal)?,
        () = cancellation.cancelled() => return Ok(()),
    }

    cancellation.cancel();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn graceful_cancellation_finishes_the_runtime() {
        let cancellation = CancellationToken::new();
        let runtime = tokio::spawn(run_until_cancelled(cancellation.clone()));
        cancellation.cancel();

        let result = tokio::time::timeout(Duration::from_secs(1), runtime)
            .await
            .expect("runtime did not stop after cancellation")
            .expect("runtime task panicked");
        assert!(result.is_ok());
    }
}
