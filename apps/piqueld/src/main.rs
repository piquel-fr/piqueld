//! Process entry point for the piqueld daemon.

use anyhow::{Context, Result};
use clap::Parser;
use piqueld::api::ApiState;
use piqueld::config::{ConfigError, DaemonConfig};
use piqueld::docker::{BollardDocker, DockerApi};
use piqueld::operations::OperationScheduler;
use piqueld::reconcile::{DockerRuntime, ReconcileHandler, run_coordinator};
use piqueld::store::SqliteStore;
use piqueld_core::InstanceId;
use std::{
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::PathBuf,
    sync::Arc,
};
use tokio::net::{TcpListener, UnixListener};
use tokio_util::sync::CancellationToken;

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

    let store = Arc::new(
        SqliteStore::open(config.server.database_path())
            .await
            .context("failed to open control-plane state")?,
    );

    let docker = connect_docker(&config.docker).await?;

    let instance = InstanceId::parse(store.instance_id().to_owned())
        .context("stored instance identity is invalid")?;

    let wake = Arc::new(tokio::sync::Notify::new());

    let runtime = Arc::new(DockerRuntime::new(
        Arc::clone(&docker),
        instance,
        Arc::clone(&wake),
        std::time::Duration::from_secs(config.reconciliation.prepare_timeout_seconds),
    ));

    let handler = Arc::new(
        ReconcileHandler::new(Arc::clone(&docker), Arc::clone(&store)).with_retry_policy(
            piqueld::reconcile::RetryPolicy {
                convergence_timeout: std::time::Duration::from_secs(
                    config.reconciliation.convergence_timeout_seconds,
                ),
                ..piqueld::reconcile::RetryPolicy::default()
            },
        ),
    );

    let scheduler = Arc::new(OperationScheduler::new(
        Arc::clone(&store),
        handler,
        config.reconciliation.max_parallel_operations,
    ));

    let state = ApiState::new(Arc::clone(&store), runtime);

    // cancellation token for workers
    let cancellation = CancellationToken::new();

    // worker to run reconciliations
    let controller_token = cancellation.child_token();
    let controller_cancellation = cancellation.clone();
    let scan_interval = std::time::Duration::from_secs(config.reconciliation.scan_interval_seconds);
    let finished_operation_days = config.retention.finished_operation_days;
    let controller = tokio::spawn(async move {
        let result = run_coordinator(
            scheduler,
            Arc::clone(&store),
            Arc::clone(&docker),
            Arc::clone(&wake),
            scan_interval,
            finished_operation_days,
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

    let tcp_api = match config.server.http_listen {
        Some(address) => Some(
            spawn_tcp_api(address, state.clone(), cancellation.clone())
                .await
                .with_context(|| format!("failed to bind HTTP API on {address}"))?,
        ),
        None => None,
    };
    let unix_api = spawn_unix_api(config.server.socket_path(), state, cancellation.clone()).await?;

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
    Ok(docker)
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
                "{} is absent; using validated built-in defaults. Developers can select the shipped example with --config config/piqueld.example.toml",
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

async fn spawn_tcp_api(
    address: std::net::SocketAddr,
    state: ApiState,
    cancellation: CancellationToken,
) -> Result<tokio::task::JoinHandle<Result<(), std::io::Error>>> {
    let listener = TcpListener::bind(address)
        .await
        .context("failed to bind HTTP API")?;
    Ok(tokio::spawn(async move {
        let shutdown = cancellation.clone();
        let serve = std::future::IntoFuture::into_future(
            axum::serve(listener, piqueld::api::router(state))
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
    }))
}

async fn spawn_unix_api(
    path: PathBuf,
    state: ApiState,
    cancellation: CancellationToken,
) -> Result<tokio::task::JoinHandle<Result<(), std::io::Error>>> {
    match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) if metadata.file_type().is_socket() => {
            tokio::fs::remove_file(&path)
                .await
                .context("failed to replace stale Unix socket")?;
        }
        Ok(_) => anyhow::bail!("refusing to replace non-socket Unix API path"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("failed to inspect Unix API path"),
    }
    let listener = UnixListener::bind(&path).context("failed to bind Unix API")?;
    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .await
        .context("failed to restrict Unix API socket permissions")?;
    Ok(tokio::spawn(async move {
        let shutdown = cancellation.clone();
        let serve = std::future::IntoFuture::into_future(
            axum::serve(listener, piqueld::api::router(state))
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
    }))
}
