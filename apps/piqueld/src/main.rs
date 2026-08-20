//! Process entry point for the piqueld daemon.

use anyhow::{Context, Result};
use clap::Parser;
use piqueld::api::ApiState;
use piqueld::config::{ConfigError, DaemonConfig};
use piqueld::docker::{BollardDocker, DockerApi};
use piqueld::operations::OperationScheduler;
use piqueld::reconcile::{DockerRuntime, ReconcileHandler, run_coordinator};
use piqueld::secrets::{MasterKey, SecretService};
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

    let store = Arc::new(
        SqliteStore::open(&config.database.path)
            .await
            .context("failed to open control-plane state")?,
    );

    let secret_service = config
        .credentials
        .encryption_key
        .as_ref()
        .map(MasterKey::load)
        .transpose()
        .context("failed to load logical-secret encryption key")?
        .map(|key| Arc::new(SecretService::new(Arc::clone(&store), key)));

    let docker = {
        let docker = Arc::new(
            BollardDocker::connect(&config.docker.socket)
                .context("failed to connect to Docker Engine")?,
        );
        docker
            .ensure_swarm(config.docker.auto_initialize_swarm)
            .await
            .context("Docker Engine is not an active single-node Swarm manager")?;
        docker
    };

    let instance = InstanceId::parse(store.instance_id().to_owned())
        .context("stored instance identity is invalid")?;

    let wake = Arc::new(tokio::sync::Notify::new());

    let mut runtime = DockerRuntime::new(Arc::clone(&docker), instance, Arc::clone(&wake));
    if let Some(service) = &secret_service {
        runtime = runtime.with_secret_service(Arc::clone(service));
    }
    let runtime = Arc::new(runtime);

    let mut handler = ReconcileHandler::new(Arc::clone(&docker), Arc::clone(&store));
    if let Some(service) = &secret_service {
        handler = handler.with_secret_service(Arc::clone(service));
    }
    let handler = Arc::new(handler);

    let scheduler = Arc::new(OperationScheduler::new(
        Arc::clone(&store),
        handler,
        config.reconciliation.max_parallel_operations,
    ));

    let ui_dir = config
        .server
        .ui_dir
        .clone()
        .unwrap_or_else(piqueld::config::default_ui_dir);
    let mut state = ApiState::new(Arc::clone(&store), runtime).with_ui_dir(ui_dir);
    if let Some(service) = &secret_service {
        state = state.with_secret_service(Arc::clone(service));
    }

    // cancellation token for workers
    let cancellation = CancellationToken::new();

    // worker to run reconciliations
    let controller_token = cancellation.child_token();
    let controller_cancellation = cancellation.clone();
    let scan_interval = std::time::Duration::from_secs(config.reconciliation.scan_interval_seconds);
    let controller = tokio::spawn(async move {
        let result = run_coordinator(
            scheduler,
            Arc::clone(&store),
            Arc::clone(&docker),
            Arc::clone(&wake),
            scan_interval,
            controller_token,
            secret_service,
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

    let tcp_api = spawn_tcp_api(
        config.server.http_listen,
        state.clone(),
        cancellation.clone(),
    )
    .await?;
    let unix_api = spawn_unix_api(config.server.unix_socket, state, cancellation.clone()).await?;

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
    controller
        .await
        .context("reconciliation controller failed")?
        .context("reconciliation controller stopped unexpectedly")?;
    Ok(())
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
        let result = axum::serve(listener, piqueld::api::router(state))
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await;
        cancellation.cancel();
        result
    }))
}

async fn spawn_unix_api(
    path: PathBuf,
    state: ApiState,
    cancellation: CancellationToken,
) -> Result<tokio::task::JoinHandle<Result<(), std::io::Error>>> {
    if let Some(parent) = path.parent() {
        if parent == std::path::Path::new("/") {
            anyhow::bail!("Unix API socket must be inside a private directory");
        }
        let parent_missing = match tokio::fs::symlink_metadata(parent).await {
            Ok(_) => false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => return Err(error).context("failed to inspect Unix socket directory"),
        };
        tokio::fs::create_dir_all(parent)
            .await
            .context("failed to create Unix socket directory")?;
        if parent_missing {
            tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .await
                .context("failed to restrict Unix socket directory permissions")?;
        }
    }
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
    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660))
        .await
        .context("failed to restrict Unix API socket permissions")?;
    Ok(tokio::spawn(async move {
        let shutdown = cancellation.clone();
        let result = axum::serve(listener, piqueld::api::api_router(state))
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await;
        cancellation.cancel();
        result
    }))
}
