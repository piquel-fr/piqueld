//! Daemon bootstrap and internal module boundaries.

pub mod api;
pub mod application;
/// Passkey authentication and durable sessions.
pub mod auth;
/// Executor-independent durable build recording.
pub mod build;
pub mod config;
pub mod dns;
pub mod docker;
pub mod ingress;
pub mod operations;
pub mod reconcile;
mod secrets;
pub mod store;
pub mod tailnet;
pub mod telemetry;

mod certificate;
mod command;
mod directories;
mod git;
mod runtime_dir;
mod ui_bundle;

use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub use directories::{DirectoryLock, prepare_data_dir};
pub use runtime_dir::RuntimeDir;

/// Errors produced by the daemon's process-level runtime skeleton.
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// Installing a process signal handler failed.
    #[error("could not install the operating-system signal handler")]
    Signal(#[source] std::io::Error),
}

/// Waits until the supplied cancellation token is cancelled, logging when
/// the daemon starts and when shutdown is requested.
///
/// Long-running controller tasks receive child tokens rooted at this token.
pub async fn run_until_cancelled(cancellation: CancellationToken) {
    info!("daemon started");
    cancellation.cancelled().await;
    info!("shutdown requested");
}

/// Converts the first SIGINT or SIGTERM into cooperative daemon cancellation,
/// and exits the process at once on the next one, even when the daemon was
/// cancelled otherwise, so a stuck shutdown, including its final telemetry
/// flush, can always be interrupted. Never returns unless the handlers cannot
/// be installed; leave it running until the process exits.
///
/// # Errors
///
/// Returns an error if the operating-system signal handlers cannot be installed.
pub async fn handle_shutdown_signals(cancellation: CancellationToken) -> Result<(), RuntimeError> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut interrupt = signal(SignalKind::interrupt()).map_err(RuntimeError::Signal)?;
    let mut terminate = signal(SignalKind::terminate()).map_err(RuntimeError::Signal)?;
    let mut shutdown_signal = async || {
        tokio::select! {
            _ = interrupt.recv() => {},
            _ = terminate.recv() => {},
        }
    };
    tokio::select! {
        () = shutdown_signal() => cancellation.cancel(),
        () = cancellation.cancelled() => {},
    }
    shutdown_signal().await;
    tracing::error!("second shutdown signal received; exiting without finishing shutdown");
    std::process::exit(1)
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

        tokio::time::timeout(Duration::from_secs(1), runtime)
            .await
            .expect("runtime did not stop after cancellation")
            .expect("runtime task panicked");
    }
}
