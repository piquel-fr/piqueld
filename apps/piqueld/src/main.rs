//! Process entry point for the piqueld daemon.

use anyhow::{Context, Result};
use piqueld::api::{ApiState, UnavailableRuntime};
use piqueld::config::DaemonConfig;
use piqueld::store::{OperationRepository, SqliteStore};
use std::{os::unix::fs::FileTypeExt, path::PathBuf, sync::Arc};
use tokio::net::{TcpListener, UnixListener};
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
    let store = Arc::new(
        SqliteStore::open(&config.database.path)
            .await
            .context("failed to open control-plane state")?,
    );
    store
        .recover_interrupted()
        .await
        .context("failed to recover interrupted operations")?;

    let state = ApiState::new(Arc::clone(&store), Arc::new(UnavailableRuntime));

    let tcp = TcpListener::bind(config.server.http_listen)
        .await
        .context("failed to bind HTTP API")?;
    let unix = {
        if let Some(parent) = config.server.unix_socket.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .context("failed to create Unix socket directory")?;
        }
        match tokio::fs::symlink_metadata(&config.server.unix_socket).await {
            Ok(metadata) if metadata.file_type().is_socket() => {
                tokio::fs::remove_file(&config.server.unix_socket)
                    .await
                    .context("failed to replace stale Unix socket")?;
            }
            Ok(_) => anyhow::bail!("refusing to replace non-socket Unix API path"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("failed to inspect Unix API path"),
        }
        UnixListener::bind(&config.server.unix_socket).context("failed to bind Unix API")?
    };

    let cancellation = CancellationToken::new();
    let signal_task = tokio::spawn(piqueld::cancel_on_shutdown_signal(cancellation.clone()));

    let tcp_api = {
        let tcp_cancel = cancellation.child_token();
        let tcp_state = state.clone();
        tokio::spawn(async move {
            axum::serve(tcp, piqueld::api::router(tcp_state))
                .with_graceful_shutdown(async move { tcp_cancel.cancelled().await })
                .await
        })
    };

    let unix_api = {
        let unix_cancel = cancellation.child_token();
        tokio::spawn(async move {
            axum::serve(unix, piqueld::api::router(state))
                .with_graceful_shutdown(async move { unix_cancel.cancelled().await })
                .await
        })
    };

    piqueld::run_until_cancelled(cancellation).await?;
    signal_task.await.context("shutdown task failed")??;
    tcp_api
        .await
        .context("TCP API task failed")?
        .context("TCP API failed")?;
    unix_api
        .await
        .context("Unix API task failed")?
        .context("Unix API failed")?;
    Ok(())
}
