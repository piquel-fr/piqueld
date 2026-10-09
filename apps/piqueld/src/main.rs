//! Process entry point for the piqueld daemon.

use anyhow::{Context, Result};
use axum::{extract::connect_info::Connected, serve::IncomingStream};
use clap::Parser;
use piqueld::api::ApplicationService;
use piqueld::api::http::{ApiState, UiAssets};
use piqueld::config::{ConfigError, DaemonConfig};
use piqueld::tailnet::Node;
use std::{net::SocketAddr, os::unix::fs::OpenOptionsExt, path::PathBuf};
use tokio::net::UnixListener;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Configuration read when `--config` is not supplied.
const DEFAULT_CONFIG_PATH: &str = "/etc/piqueld/config.toml";
/// Time open connections get to finish after shutdown is requested.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

// Command-line arguments. Doc comments on fields become CLI help text.
#[derive(Debug, Parser)]
#[command(
    name = "piqueld",
    version,
    about = "Run the piqueld single-node Docker control plane"
)]
struct Args {
    /// Read daemon configuration from this TOML file.
    ///
    /// Without this flag, `/etc/piqueld/config.toml` is read if it exists;
    /// otherwise built-in defaults are used.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Also append JSON logs to this file, created with mode `0600`.
    ///
    /// Standard output keeps its usual format, so tools can query the logs
    /// while a person watches the terminal.
    #[arg(long, value_name = "PATH")]
    log_file: Option<PathBuf>,
}

impl Args {
    /// Opens `--log-file` for appending, creating it private when missing.
    fn open_log_file(&self) -> Result<Option<std::fs::File>> {
        self.log_file
            .as_ref()
            .map(|path| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .mode(0o600)
                    .open(path)
                    .with_context(|| format!("failed to open log file {}", path.display()))
            })
            .transpose()
    }
}

/// Starts the daemon and runs until a shutdown signal.
///
/// 1. Loads configuration and initializes tracing.
/// 2. Prepares and locks the data directory, then validates and locks the runtime
///    directory and binds every listener, so misconfiguration fails before any
///    state is opened.
/// 3. Starts the application service and reconciliation controller.
/// 4. Serves the dashboard/API over TCP, metrics, and the Unix API socket.
/// 5. After cancellation, waits for every task and pending audit record, and
///    surfaces the first failure.
#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let mut config = load_config(args.config.as_deref())?;
    piqueld::config::init_tracing(args.open_log_file()?).context("failed to initialize tracing")?;

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
    let metrics_listeners = config.metrics.bind().await?;
    // The node may fill auth.public_url, so it joins before authentication starts.
    let tailnet = Node::join(&mut config).await?;

    let cancellation = CancellationToken::new();
    let (state, auth, controller) =
        match ApplicationService::start(&config, cancellation.clone()).await {
            Ok(started) => started,
            Err(error) => {
                if let Some(node) = tailnet {
                    node.leave().await;
                }
                return Err(error);
            }
        };
    let state = state.with_tailnet(tailnet.as_ref().map(Node::status));
    let ui_assets = UiAssets::resolve();
    log_ui_status(&ui_assets);

    // OS signal handling
    let signal_cancellation = cancellation.clone();
    let signal_task = tokio::spawn(async move {
        let result = piqueld::cancel_on_shutdown_signal(signal_cancellation.clone()).await;
        signal_cancellation.cancel();
        result
    });

    let web_router = |hosts| {
        piqueld::api::http::web_router_with_hosts(state.clone(), ui_assets, auth.clone(), hosts)
    };
    let mut tcp_apis: Vec<_> = tcp_listeners
        .into_iter()
        .map(|listener| {
            spawn_tcp_api(
                listener,
                web_router(config.server.allowed_hosts.clone()),
                cancellation.clone(),
            )
        })
        .collect();
    let tailnet_supervisor = tailnet.map(|node| {
        let mut hosts = config.server.allowed_hosts.clone();
        hosts.push(node.dns_name().to_owned());
        // Only this listener's requests carry a tailnet identity.
        let whois: std::sync::Arc<dyn piqueld::tailnet::TailnetLookup> = node.whois();
        let (listener, supervisor) = node.listener(cancellation.clone());
        tcp_apis.push(spawn_tcp_api(
            listener,
            web_router(hosts).layer(axum::Extension(whois)),
            cancellation.clone(),
        ));
        supervisor
    });
    let metrics_apis: Vec<_> = metrics_listeners
        .into_iter()
        .map(|listener| {
            spawn_tcp_api(
                listener,
                piqueld::api::http::metrics_router(state.clone()),
                cancellation.clone(),
            )
        })
        .collect();
    let audit = state.clone();
    let unix_api = spawn_unix_api(unix_listener, state, auth, cancellation.clone());

    piqueld::run_until_cancelled(cancellation).await?;

    signal_task.await.context("shutdown task failed")??;
    for tcp_api in tcp_apis.into_iter().chain(metrics_apis) {
        tcp_api
            .await
            .context("TCP API task failed")?
            .context("TCP API failed")?;
    }
    unix_api
        .await
        .context("Unix API task failed")?
        .context("Unix API failed")?;
    audit.drain_audit(SHUTDOWN_GRACE).await;
    controller
        .await
        .context("reconciliation controller failed")?
        .context("reconciliation controller stopped unexpectedly")?;
    if let Some(supervisor) = tailnet_supervisor {
        supervisor.await.context("tailnet supervisor failed")??;
    }
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

/// Loads configuration from `--config` when given, where any failure is fatal.
/// Otherwise reads `DEFAULT_CONFIG_PATH`, falling back to validated built-in
/// defaults only when that file does not exist.
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

/// Serves `router` on a TCP-style listener (plain TCP or the tailnet node's
/// forwarded connections) until cancellation, recording peer addresses for throttling.
/// In-flight connections get `SHUTDOWN_GRACE` to finish, and the task cancels
/// the whole daemon when it exits for any reason.
fn spawn_tcp_api<L>(
    listener: L,
    router: axum::Router,
    cancellation: CancellationToken,
) -> tokio::task::JoinHandle<Result<(), std::io::Error>>
where
    L: axum::serve::Listener,
    L::Addr: std::fmt::Debug,
    for<'a> SocketAddr: Connected<IncomingStream<'a, L>>,
{
    info!(address = ?listener.local_addr(), "HTTP API listening");
    tokio::spawn(async move {
        let shutdown = cancellation.clone();
        let serve = std::future::IntoFuture::into_future(
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
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

/// Serves the authenticated API on the Unix socket with the same shutdown
/// behavior as [`spawn_tcp_api`].
fn spawn_unix_api(
    listener: UnixListener,
    state: ApiState,
    auth: piqueld::auth::Auth,
    cancellation: CancellationToken,
) -> tokio::task::JoinHandle<Result<(), std::io::Error>> {
    tokio::spawn(async move {
        let shutdown = cancellation.clone();
        let serve = std::future::IntoFuture::into_future(
            axum::serve(
                listener,
                piqueld::api::http::api_router(state, auth)
                    .into_make_service_with_connect_info::<piqueld::api::http::UnixPeer>(),
            )
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
