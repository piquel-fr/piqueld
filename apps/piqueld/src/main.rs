//! Process entry point for the piqueld daemon.

use anyhow::{Context, Result};
use piqueld::config::DaemonConfig;
use piqueld::store::{LibsqlStore, OperationRepository};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args_os().any(|argument| argument == "--version") {
        println!("piqueld {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let config_path = std::env::var_os("PIQUELD_CONFIG")
        .map_or_else(|| PathBuf::from("/etc/piqueld/config.toml"), PathBuf::from);
    let config = DaemonConfig::load(&config_path).with_context(|| {
        format!(
            "failed to load configuration from {}",
            config_path.display()
        )
    })?;
    piqueld::config::init_tracing().context("failed to initialize tracing")?;
    let store = LibsqlStore::open(&config.database.path)
        .await
        .context("failed to open control-plane state")?;
    store
        .recover_interrupted()
        .await
        .context("failed to recover interrupted operations")?;

    let cancellation = CancellationToken::new();
    let signal_task = tokio::spawn(piqueld::cancel_on_shutdown_signal(cancellation.clone()));
    piqueld::run_until_cancelled(cancellation).await?;
    signal_task.await.context("shutdown task failed")??;
    Ok(())
}
