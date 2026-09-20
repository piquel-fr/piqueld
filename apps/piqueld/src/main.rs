//! Process entry point for the piqueld daemon.

use anyhow::{Context, Result};
use clap::Parser;
use piqueld::api::{ApiState, UiAssets};
use piqueld::application::ApplicationService;
use piqueld::config::{ConfigError, DaemonConfig};
use std::path::PathBuf;
use tokio::net::{TcpListener, UnixListener};
use tokio_util::sync::CancellationToken;
use tracing::info;

const DEFAULT_CONFIG_PATH: &str = "/etc/piqueld/config.toml";
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, Parser)]
#[command(
    name = "piqueld",
    version,
    about = "Run the piqueld single-node Docker control plane"
)]
struct Args {
    /// Read daemon configuration from this TOML file.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let config = load_config(args.config.as_deref())?;
    piqueld::config::init_tracing().context("failed to initialize tracing")?;

    piqueld::prepare_data_dir(&config.server.data_dir)
        .await
        .with_context(|| {
            format!(
                "failed to prepare data directory {}",
                config.server.data_dir.display()
            )
        })?;

    let _lock = piqueld::DirectoryLock::acquire(&config.server.data_dir).with_context(|| {
        format!(
            "failed to lock data directory {}",
            config.server.data_dir.display()
        )
    })?;
    // Bind all endpoints before opening state or starting background work.
    let runtime_dir = piqueld::RuntimeDir::acquire(&config.server.runtime_dir).await?;
    let tcp_listeners = config.server.bind_tcp().await?;
    let unix_listener = runtime_dir.bind_api().await?;

    let cancellation = CancellationToken::new();
    let (state, controller) = ApplicationService::start(&config, cancellation.clone()).await?;
    let ui_assets = UiAssets::resolve();
    log_ui_status(&ui_assets);

    // OS signal handling
    let signal_cancellation = cancellation.clone();
    let signal_task = tokio::spawn(async move {
        let result = piqueld::cancel_on_shutdown_signal(signal_cancellation.clone()).await;
        signal_cancellation.cancel();
        result
    });

    let tcp_apis: Vec<_> = tcp_listeners
        .into_iter()
        .map(|listener| spawn_tcp_api(listener, state.clone(), ui_assets, cancellation.clone()))
        .collect();
    let unix_api = spawn_unix_api(unix_listener, state, cancellation.clone());

    piqueld::run_until_cancelled(cancellation).await?;

    signal_task.await.context("shutdown task failed")??;
    for tcp_api in tcp_apis {
        tcp_api
            .await
            .context("TCP API task failed")?
            .context("TCP API failed")?;
    }
    unix_api
        .await
        .context("Unix API task failed")?
        .context("Unix API failed")?;
    controller
        .await
        .context("reconciliation controller failed")?
        .context("reconciliation controller stopped unexpectedly")?;
    Ok(())
}

/// Reports dashboard availability once at startup so a binary without the
/// embedded bundle is identifiable in the log without probing `/dashboard`.
fn log_ui_status(ui_assets: &UiAssets) {
    match ui_assets {
        UiAssets::Disabled => info!("dashboard disabled: built without the embedded-ui feature"),
        UiAssets::Embedded(bundle) => {
            info!(
                assets = bundle.len(),
                "dashboard enabled: bundle is embedded"
            );
        }
    }
}

fn load_config(explicit_path: Option<&std::path::Path>) -> Result<DaemonConfig> {
    if let Some(path) = explicit_path {
        return DaemonConfig::load(path).with_context(|| {
            format!(
                "failed to load explicitly supplied configuration from {}",
                path.display()
            )
        });
    }

    let default_path = PathBuf::from(DEFAULT_CONFIG_PATH);
    match DaemonConfig::load(&default_path) {
        Ok(config) => Ok(config),
        Err(ConfigError::Read(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "{} is absent; using validated built-in defaults. Developers can select the shipped example with --config examples/piqueld.toml",
                default_path.display()
            );
            DaemonConfig::validated_default().context("validated built-in defaults are invalid")
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to load default configuration from {}",
                default_path.display()
            )
        }),
    }
}

fn spawn_tcp_api(
    listener: TcpListener,
    state: ApiState,
    ui_assets: UiAssets,
    cancellation: CancellationToken,
) -> tokio::task::JoinHandle<Result<(), std::io::Error>> {
    info!(address = ?listener.local_addr(), "HTTP API listening");
    tokio::spawn(async move {
        let shutdown = cancellation.clone();
        let serve = std::future::IntoFuture::into_future(
            axum::serve(listener, piqueld::api::web_router(state, ui_assets))
                .with_graceful_shutdown(async move { shutdown.cancelled().await }),
        );
        tokio::pin!(serve);
        // The grace period starts only once shutdown has been requested; a
        // healthy server must never be torn down by an elapsed deadline.
        let grace = async {
            cancellation.cancelled().await;
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        };
        tokio::pin!(grace);
        let served = tokio::select! {
            result = &mut serve => result,
            () = &mut grace => {
                tracing::warn!(
                    "HTTP graceful shutdown grace elapsed; closing remaining connections"
                );
                Ok(())
            }
        };
        cancellation.cancel();
        served
    })
}

fn spawn_unix_api(
    listener: UnixListener,
    state: ApiState,
    cancellation: CancellationToken,
) -> tokio::task::JoinHandle<Result<(), std::io::Error>> {
    tokio::spawn(async move {
        let shutdown = cancellation.clone();
        let serve = std::future::IntoFuture::into_future(
            axum::serve(listener, piqueld::api::api_router(state))
                .with_graceful_shutdown(async move { shutdown.cancelled().await }),
        );
        tokio::pin!(serve);
        // Same shutdown-only grace as the TCP API.
        let grace = async {
            cancellation.cancelled().await;
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        };
        tokio::pin!(grace);
        let served = tokio::select! {
            result = &mut serve => result,
            () = &mut grace => {
                tracing::warn!(
                    "Unix socket graceful shutdown grace elapsed; closing remaining connections"
                );
                Ok(())
            }
        };
        cancellation.cancel();
        served
    })
}
