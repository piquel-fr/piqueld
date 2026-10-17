//! Process entry point for the piqueld daemon.

use anyhow::{Context, Result};
use axum::{extract::connect_info::Connected, serve::IncomingStream};
use clap::Parser;
use piqueld::api::ApplicationService;
use piqueld::api::http::{self, ApiState, UiAssets, UnixPeer};
use piqueld::auth::Auth;
use piqueld::config::{ConfigError, DaemonConfig};
use piqueld::ingress::{DashboardSocket, Ingress};
use piqueld::tailnet::Node;
use piqueld::tailnet::TailnetLookup;
use std::{net::SocketAddr, os::unix::fs::OpenOptionsExt, path::PathBuf, sync::Arc};
use tokio::net::{TcpListener, UnixListener};
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Configuration read when `--config` is not supplied.
const DEFAULT_CONFIG_PATH: &str = "/etc/piqueld/config.toml";
/// A served listener, which ends after shutdown or on failure.
type ApiTask = tokio::task::JoinHandle<Result<(), std::io::Error>>;
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
    /// Runs the daemon when omitted.
    #[command(subcommand)]
    command: Option<Command>,

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

/// One-shot commands that exit instead of running the daemon.
#[derive(Debug, clap::Subcommand)]
enum Command {
    /// Check a configuration file's syntax, types, ranges, and combinations,
    /// then exit.
    ///
    /// Nothing on the host is touched: no state, sockets, Docker, or network.
    /// Credential files are only located, not read, so the check also runs
    /// where they are absent, such as a Nix build.
    CheckConfig {
        /// The TOML file to check.
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
    },
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

/// Runs a one-shot [`Command`], or the daemon until a shutdown signal.
#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    match &args.command {
        Some(Command::CheckConfig { config }) => {
            DaemonConfig::check(config)
                .with_context(|| format!("failed to check configuration {}", config.display()))?;
            println!("configuration {} is valid", config.display());
            Ok(())
        }
        None => {
            let config = load_config(args.config.as_deref())?;
            piqueld::config::init_tracing(args.open_log_file()?)
                .context("failed to initialize tracing")?;
            serve(config).await
        }
    }
}

/// Starts the daemon with loaded configuration and runs until a shutdown
/// signal. Tracing is already initialized.
///
/// 1. Prepares and locks the data directory, then validates and locks the runtime
///    directory and binds every listener, so misconfiguration fails before any
///    state is opened.
/// 2. Starts the application service and reconciliation controller.
/// 3. Serves the dashboard/API over TCP, metrics, and the Unix API socket.
/// 4. After cancellation, waits for every task and pending audit record, and
///    surfaces the first failure.
async fn serve(mut config: DaemonConfig) -> Result<()> {
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
    let listeners = Listeners::bind(&config, &runtime_dir).await?;
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

    // OS signal handling
    let signal_cancellation = cancellation.clone();
    let signal_task = tokio::spawn(async move {
        let result = piqueld::cancel_on_shutdown_signal(signal_cancellation.clone()).await;
        signal_cancellation.cancel();
        result
    });

    let audit = state.clone();
    let (apis, tailnet_supervisor) = listeners.serve(&config, state, auth, tailnet, &cancellation);

    piqueld::run_until_cancelled(cancellation).await;

    signal_task.await.context("shutdown task failed")??;
    for api in apis {
        api.await
            .context("HTTP API task failed")?
            .context("HTTP API failed")?;
    }
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

/// Every endpoint the daemon serves, bound before it opens any state, so
/// misconfiguration fails before anything else starts.
struct Listeners {
    /// The website on `server.listen_mode`'s addresses.
    tcp: Vec<TcpListener>,
    /// The API alone, on the runtime directory's socket.
    unix: UnixListener,
    /// The metrics-only listeners.
    metrics: Vec<TcpListener>,
    /// The socket the gateway forwards the dashboard hostname to.
    dashboard: Option<DashboardSocket>,
}

impl Listeners {
    async fn bind(config: &DaemonConfig, runtime_dir: &piqueld::RuntimeDir) -> Result<Self> {
        Ok(Self {
            tcp: config.server.bind_tcp().await?,
            unix: runtime_dir.bind_api().await?,
            metrics: config.metrics.bind().await?,
            dashboard: Ingress::bind_dashboard(config)
                .await
                .context("failed to bind the dashboard socket")?,
        })
    }

    /// Serves each listener, and the `tailnet` node's, with its router until
    /// `cancellation`. Returns the API tasks and the node's supervisor.
    fn serve(
        self,
        config: &DaemonConfig,
        state: ApiState,
        auth: Auth,
        tailnet: Option<Node>,
        cancellation: &CancellationToken,
    ) -> (Vec<ApiTask>, Option<tokio::task::JoinHandle<Result<()>>>) {
        let ui_assets = UiAssets::resolve();
        log_ui_status(&ui_assets);
        let web_router =
            |hosts| http::web_router_with_hosts(state.clone(), ui_assets, auth.clone(), hosts);
        let mut apis: Vec<_> = self
            .tcp
            .into_iter()
            .map(|listener| {
                spawn_api::<_, SocketAddr>(
                    listener,
                    web_router(config.server.allowed_hosts.clone()),
                    cancellation.clone(),
                )
            })
            .collect();
        // Only requests through the tailnet node, or the apps node's dashboard
        // hostname, carry a tailnet identity, looked up through that node.
        let tailnet_router = |host: String, whois: Arc<dyn TailnetLookup>| {
            let mut hosts = config.server.allowed_hosts.clone();
            hosts.push(host);
            web_router(hosts).layer(axum::Extension(whois))
        };
        if let Some(dashboard) = self.dashboard {
            let router = tailnet_router(dashboard.hostname.to_string(), dashboard.whois);
            apis.push(spawn_api::<_, UnixPeer>(
                dashboard.listener,
                http::gateway_router(router),
                cancellation.clone(),
            ));
        }
        let supervisor = tailnet.map(|node| {
            let router = tailnet_router(node.dns_name().to_owned(), node.whois());
            let (listener, supervisor) = node.listener(cancellation.clone());
            apis.push(spawn_api::<_, SocketAddr>(
                listener,
                router,
                cancellation.clone(),
            ));
            supervisor
        });
        apis.extend(self.metrics.into_iter().map(|listener| {
            spawn_api::<_, SocketAddr>(
                listener,
                http::metrics_router(state.clone()),
                cancellation.clone(),
            )
        }));
        apis.push(spawn_api::<_, UnixPeer>(
            self.unix,
            http::api_router(state, auth),
            cancellation.clone(),
        ));
        (apis, supervisor)
    }
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

/// Serves `router` on `listener` (plain TCP, the tailnet node's forwarded
/// connections, or a Unix socket) until cancellation, recording each
/// connection's `C` (its peer address, or its Unix credentials) for
/// throttling and host operator checks. In-flight connections get
/// `SHUTDOWN_GRACE` to finish, and the task cancels the whole daemon when it
/// exits for any reason.
fn spawn_api<L, C>(listener: L, router: axum::Router, cancellation: CancellationToken) -> ApiTask
where
    L: axum::serve::Listener,
    L::Addr: std::fmt::Debug,
    C: Clone + Send + Sync + 'static,
    for<'a> C: Connected<IncomingStream<'a, L>>,
{
    info!(address = ?listener.local_addr(), "HTTP API listening");
    tokio::spawn(async move {
        let shutdown = cancellation.clone();
        let serve = std::future::IntoFuture::into_future(
            axum::serve(listener, router.into_make_service_with_connect_info::<C>())
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
