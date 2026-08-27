//! Daemon bootstrap and internal module boundaries.

pub mod api;
pub mod config;
pub mod docker;
pub mod operations;
pub mod reconcile;
pub mod store;

mod ui_bundle;

use std::{
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Prepares the daemon's single state directory.
///
/// The directory holds the Unix API socket, the embedded database, and future
/// user data. Missing components are created with mode `0700`. Existing
/// components are never modified but must be real (non-symlink) directories,
/// and the final directory must grant no access to group or other users:
/// anyone able to write there could replace the daemon socket and intercept
/// operator connections.
///
/// # Errors
///
/// Returns an [`std::io::Error`] when the directory cannot be inspected or
/// prepared, or when it violates the privacy requirements.
pub async fn prepare_data_dir(path: &Path) -> std::io::Result<()> {
    if path == Path::new("/") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the data directory must be a dedicated private directory",
        ));
    }

    let mut current = std::path::PathBuf::new();
    let mut components = path.components().peekable();
    let expected_uid = rustix::process::geteuid().as_raw();
    while let Some(component) = components.next() {
        match component {
            std::path::Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            std::path::Component::RootDir => current.push(std::path::Path::new("/")),
            std::path::Component::CurDir => continue,
            std::path::Component::ParentDir => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "the data directory cannot contain a parent component",
                ));
            }
            std::path::Component::Normal(name) => current.push(name),
        }

        let is_final = components.peek().is_none();
        match tokio::fs::symlink_metadata(&current).await {
            Ok(metadata) if metadata.is_dir() && !metadata.is_symlink() => {}
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "data directory component {} is not a real directory",
                        current.display()
                    ),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tokio::fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&current)
                    .await?;
                let metadata = tokio::fs::symlink_metadata(&current).await?;
                if !metadata.is_dir() || metadata.is_symlink() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!(
                            "data directory component {} was replaced by a non-directory",
                            current.display()
                        ),
                    ));
                }
            }
            Err(error) => return Err(error),
        }

        let metadata = tokio::fs::symlink_metadata(&current).await?;
        if is_final {
            let mode = metadata.permissions().mode();
            if mode & 0o077 != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "data directory {} must be private (mode {:o} grants group or other access)",
                        current.display(),
                        mode & 0o777
                    ),
                ));
            }
            if metadata.uid() != expected_uid {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!(
                        "data directory {} must be owned by uid {expected_uid}",
                        current.display()
                    ),
                ));
            }
        } else {
            let mode = metadata.permissions().mode();
            let sticky = mode & 0o1000 != 0;
            if !sticky
                && (mode & 0o022 != 0 || (metadata.uid() != 0 && metadata.uid() != expected_uid))
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!(
                        "data directory ancestor {} is not protected from replacement by other users",
                        current.display()
                    ),
                ));
            }
        }
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
