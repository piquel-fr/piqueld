//! Daemon bootstrap and internal module boundaries.

pub mod api;
pub mod build;
pub mod config;
pub mod docker;
pub mod operations;
pub mod proxy;
pub mod reconcile;
pub mod registry;
pub mod secrets;
pub mod store;

use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::info;

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
