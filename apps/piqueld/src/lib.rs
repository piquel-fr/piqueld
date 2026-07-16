//! Daemon bootstrap and internal module boundaries.

pub mod api;
mod auth;
mod build;
pub mod config;
mod docker;
pub mod operations;
mod proxy;
mod reconcile;
mod registry;
mod secrets;
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
/// Long-running controllers added in later increments will receive child tokens
/// rooted at this token.
///
/// # Errors
///
/// Returns a runtime error if a controller cannot shut down cleanly in a later
/// implementation. The foundation wait itself is infallible.
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
        }
    }

    #[cfg(not(unix))]
    tokio::signal::ctrl_c()
        .await
        .map_err(RuntimeError::Signal)?;

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
