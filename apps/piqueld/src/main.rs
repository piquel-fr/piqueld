//! Process entry point for the piqueld daemon.

use anyhow::{Context, Result};
use clap::Parser;
use piqueld::api::{ApiState, UiAssets};
use piqueld::config::{ConfigError, DaemonConfig};
use piqueld::docker::{BollardDocker, DockerApi};
use piqueld::reconcile::Controller;
use piqueld::store::Store;
use std::{path::PathBuf, sync::Arc};
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
    // Bind both endpoints before opening state or starting any background work.
    let runtime_dir = piqueld::RuntimeDir::acquire(&config.server.runtime_dir).await?;
    let tcp_listener = match config.server.http_listen {
        Some(address) => Some(
            TcpListener::bind(address)
                .await
                .with_context(|| format!("failed to bind HTTP API on {address}"))?,
        ),
        None => None,
    };
    let unix_listener = runtime_dir.bind_api().await?;

    let store = Arc::new(
        Store::open(config.server.database_path())
            .await
            .context("failed to open control-plane state")?
            .with_build_history(config.build_history.clone()),
    );
    info!(
        path = %config.server.database_path().display(),
        "opened control-plane state"
    );

    let docker = connect_docker(&config.docker).await?;

    let wake = Arc::new(tokio::sync::Notify::new());

    let reconciler = Controller::new(Arc::clone(&docker), Arc::clone(&store)).with_retry_policy(
        piqueld::reconcile::RetryPolicy {
            convergence_timeout: std::time::Duration::from_secs(
                config.reconciliation.convergence_timeout_seconds,
            ),
            ..piqueld::reconcile::RetryPolicy::default()
        },
    );

    let reconciler = reconciler.with_prepare_timeout(std::time::Duration::from_secs(
        config.reconciliation.prepare_timeout_seconds,
    ));
    let runtime = reconciler.runtime(Arc::clone(&wake));
    let ui_assets = UiAssets::resolve();
    log_ui_status(&ui_assets);
    let state = ApiState::new(Arc::clone(&store), runtime).with_configuration(config.view());

    // cancellation token for workers
    let cancellation = CancellationToken::new();

    // worker to run reconciliations
    let controller_token = cancellation.child_token();
    let controller_cancellation = cancellation.clone();
    let scan_interval = std::time::Duration::from_secs(config.reconciliation.scan_interval_seconds);
    let finished_operation_days = config.retention.finished_operation_days;
    let event_days = config.retention.event_days;
    let controller = tokio::spawn(async move {
        let result = reconciler
            .run(
                wake,
                scan_interval,
                finished_operation_days,
                event_days,
                controller_token,
            )
            .await;
        controller_cancellation.cancel();
        result
    });

    // OS signal handling
    let signal_cancellation = cancellation.clone();
    let signal_task = tokio::spawn(async move {
        let result = piqueld::cancel_on_shutdown_signal(signal_cancellation.clone()).await;
        signal_cancellation.cancel();
        result
    });

    let tcp_api = tcp_listener
        .map(|listener| spawn_tcp_api(listener, state.clone(), ui_assets, cancellation.clone()));
    let unix_api = spawn_unix_api(unix_listener, state, cancellation.clone());

    piqueld::run_until_cancelled(cancellation).await?;

    signal_task.await.context("shutdown task failed")??;
    if let Some(tcp_api) = tcp_api {
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

async fn connect_docker(config: &piqueld::config::DockerConfig) -> Result<Arc<BollardDocker>> {
    let docker = Arc::new(
        BollardDocker::connect(&config.socket).context("failed to connect to Docker Engine")?,
    );
    docker
        .ensure_swarm(config.auto_initialize_swarm)
        .await
        .context("Docker Engine is not an active single-node Swarm manager")?;
    info!(
        socket = %config.socket.display(),
        auto_initialize_swarm = config.auto_initialize_swarm,
        "connected to Docker Engine as a single-node Swarm manager"
    );
    Ok(docker)
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
