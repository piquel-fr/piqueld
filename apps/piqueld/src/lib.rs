//! Daemon bootstrap and internal module boundaries.

pub mod api;
pub mod config;
pub mod docker;
pub mod operations;
pub mod reconcile;
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

/// Waits for a shutdown signal or cancellation, then cancels the provided token.
///
/// On Unix, shutdown signals are SIGINT and SIGTERM. On other platforms, the shutdown
/// signal is Ctrl-C. If the token is already cancelled or becomes cancelled while waiting,
/// the function returns successfully without waiting for an operating-system signal.
///
/// # Errors
///
/// Returns an error if the operating-system signal handler or Ctrl-C handler cannot be
/// installed or queried.
///
/// # Examples
///
/// ```no_run
/// # use tokio_util::sync::CancellationToken;
/// # #[tokio::main]
/// # async fn main() -> Result<(), piqueld::RuntimeError> {
/// let cancellation = CancellationToken::new();
/// let wait = piqueld::cancel_on_shutdown_signal(cancellation.clone());
///
/// cancellation.cancel();
/// wait.await?;
/// # Ok(())
/// # }
/// ```
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
