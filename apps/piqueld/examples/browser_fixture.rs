//! Isolated UI/API fixture for Playwright. Docker execution is deliberately
//! stubbed: nothing ever deploys, and commands in service `web` run a
//! scripted terminal instead. With `PIQUELD_PLAYWRIGHT_FAKE_DOCKER=1`, the real
//! controller deploys to the in-memory engine of [`piqueld::docker::fake`]
//! instead, so deployments succeed and services report healthy, as the
//! documentation screenshots show them. Webhook URLs show on
//! `hooks.example.com`, as the guides configure it, though nothing listens
//! there.
use anyhow::Context as _;
use async_trait::async_trait;
use futures_util::StreamExt as _;
use piqueld::{
    api::http::{ApiState, UiAssets},
    application::{BoundaryError, RuntimeBoundary},
    docker::{DockerError, Exec, ExecIo, fake::FakeDocker},
    reconcile::Controller,
    store::{Store, StoredEnvironment},
};
use piqueld_core::{
    EnvironmentId, NormalizedApplication, ObservedApplication, ResolutionSet,
    exec::{ExecInput, ExecOutput, ExecRequest},
    manifest::Hostname,
    resource::ResolvedApplication,
};
use std::sync::Arc;
use tokio::task::JoinHandle;
struct Runtime;
#[async_trait]
impl RuntimeBoundary for Runtime {
    async fn logs(
        &self,
        _: &EnvironmentId,
        _: Option<&str>,
        _: u16,
        _: u32,
        _: Option<piqueld_core::api::LogStream>,
    ) -> Result<piqueld_core::api::ApplicationLogs, BoundaryError> {
        Ok(piqueld_core::api::ApplicationLogs::default())
    }
    async fn prepare(
        &self,
        _: &EnvironmentId,
        _: &NormalizedApplication,
        _: &ResolutionSet,
    ) -> Result<ResolvedApplication, BoundaryError> {
        Err(DockerError::Unavailable("browser fixture").into())
    }
    async fn check_available(&self) -> Result<(), BoundaryError> {
        Ok(())
    }
    async fn local_images(&self) -> Result<piqueld_core::LocalImages, BoundaryError> {
        Err(DockerError::Unavailable("browser fixture").into())
    }
    async fn reuse_images(
        &self,
        _: &std::collections::BTreeMap<piqueld_core::ServiceName, piqueld_core::ResolvedSource>,
    ) -> Result<piqueld::reconcile::ImagesInUse, BoundaryError> {
        Err(DockerError::Unavailable("browser fixture").into())
    }
    async fn observe(&self, _: &StoredEnvironment) -> Result<ObservedApplication, BoundaryError> {
        Ok(ObservedApplication::default())
    }
    /// The fixture runs no containers.
    async fn containers(&self) -> Result<Vec<piqueld::docker::ManagedContainer>, BoundaryError> {
        Ok(Vec::new())
    }
    async fn container_usage(
        &self,
        _: &str,
    ) -> Result<Option<piqueld::docker::ContainerUsage>, BoundaryError> {
        Ok(None)
    }
    fn container_logs(&self, _: &str, _: i32) -> piqueld::docker::ContainerLogs {
        futures_util::stream::empty().boxed()
    }
    /// No container ever starts.
    fn container_starts(&self, _: i64) -> piqueld::docker::ContainerStarts {
        futures_util::stream::pending().boxed()
    }
    async fn readiness(&self) -> (bool, bool) {
        (false, false)
    }
    /// Nothing else mutates the fixture's runtime.
    async fn lock_mutations(&self) -> tokio::sync::OwnedMutexGuard<()> {
        Arc::new(tokio::sync::Mutex::new(())).lock_owned().await
    }
    async fn remove_secrets(
        &self,
        _: &EnvironmentId,
        names: &[String],
    ) -> Result<(), BoundaryError> {
        if names.is_empty() {
            Ok(())
        } else {
            Err(DockerError::Unavailable("browser fixture").into())
        }
    }
    /// Only `web` has a running task; other services are not running.
    async fn create_exec(
        &self,
        _: &EnvironmentId,
        request: &ExecRequest,
    ) -> Result<Option<Exec>, BoundaryError> {
        Ok((request.service.as_str() == "web").then(|| Exec {
            id: "fixture-exec".into(),
            task: "fixture-task".into(),
            tty: request.tty.is_some(),
        }))
    }
    /// Echoes input, reports resizes, and exits with code 3 at Ctrl-D.
    async fn run_exec(&self, _: &Exec, mut io: ExecIo) -> Result<i64, BoundaryError> {
        let mut output = vec![b"fixture ready\r\n".to_vec()];
        loop {
            for data in output.drain(..) {
                if io.output.send(ExecOutput::Stdout(data)).await.is_err() {
                    return Err(DockerError::Request("client disconnected").into());
                }
            }
            match io.input.recv().await {
                Some(ExecInput::Stdin(data)) if data.contains(&4) => return Ok(3),
                Some(ExecInput::Stdin(data)) => output.push(data),
                Some(ExecInput::Resize(size)) => output.push(
                    format!("\r\nresized to {}x{}\r\n", size.width, size.height).into_bytes(),
                ),
                Some(ExecInput::CloseStdin) => {}
                None => return Err(DockerError::Request("client disconnected").into()),
            }
        }
    }
    fn trigger_reconciliation(&self) {}
}

/// What the fixture's API deploys to: the fake Docker engine when
/// `PIQUELD_PLAYWRIGHT_FAKE_DOCKER` is `1` or `true`, nothing otherwise.
enum Backend {
    /// The stubbed [`Runtime`]: nothing ever deploys.
    Stub,
    /// The real controller, deploying to [`FakeDocker`], showing webhook URLs
    /// on `hooks.example.com`.
    FakeDocker,
}

impl Backend {
    fn from_env() -> Self {
        match std::env::var("PIQUELD_PLAYWRIGHT_FAKE_DOCKER").as_deref() {
            Ok("1" | "true") => Self::FakeDocker,
            _ => Self::Stub,
        }
    }

    /// The API over `store`, and the work behind it, which runs until aborted
    /// and returns only once it fails.
    fn serve(self, store: Arc<Store>) -> (ApiState, JoinHandle<anyhow::Result<()>>) {
        let Self::FakeDocker = self else {
            let state = ApiState::new(store, Arc::new(Runtime));
            return (state, tokio::spawn(std::future::pending()));
        };
        let config = piqueld::config::DaemonConfig::default();
        let controller = Controller::new(Arc::new(FakeDocker::default()), Arc::clone(&store))
            .with_config(&config.reconciliation);
        let wake = Arc::new(tokio::sync::Notify::new());
        let state = ApiState::new(store, controller.runtime(Arc::clone(&wake)))
            .with_image_status(controller.image_status())
            .with_webhook_hostname(Hostname::parse("hooks.example.com").ok());
        let run = tokio::spawn(async move {
            let cancellation = tokio_util::sync::CancellationToken::new();
            // Image cleanup runs until cancelled: stop it once the
            // reconciler stops, so its error ends the fixture.
            let reconcile = async {
                let result = controller
                    .run(
                        wake,
                        std::time::Duration::from_secs(config.reconciliation.scan_interval_seconds),
                        config.retention.finished_operation_days,
                        config.retention.event_days,
                        cancellation.child_token(),
                    )
                    .await;
                cancellation.cancel();
                result
            };
            let (result, ()) = tokio::join!(
                reconcile,
                controller.run_image_cleanup(cancellation.child_token()),
            );
            result.context("reconciling against the fake Docker engine")
        });
        (state, run)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = std::path::PathBuf::from(std::env::var("PIQUELD_PLAYWRIGHT_DATA_DIR")?);
    let store = Arc::new(Store::open(dir.join("db")).await?);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("http://localhost:{}", listener.local_addr()?.port());
    let auth = piqueld::auth::Auth::new(&store, &origin)?;
    let setup = dir.join("setup-link");
    auth.prepare_setup(&setup).await?;
    // Installation checks inspect these settings; the fixture serves no
    // runtime socket, so its API socket check fails, as a real failure would.
    let config = piqueld::config::DaemonConfig::from_toml(&format!(
        "[server]\ndata_dir = '{}'\nruntime_dir = '{}'\n[auth]\npublic_url = '{origin}'",
        dir.join("data").display(),
        dir.display()
    ))?;
    let (state, mut controller) = Backend::from_env().serve(store);
    let state = state.with_configuration(&config);
    let router = piqueld::api::http::web_router(state.clone(), UiAssets::resolve(), auth.clone());
    let mut server = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
    });
    let socket = dir.join("api.sock");
    let unix = tokio::net::UnixListener::bind(&socket)?;
    let router = piqueld::api::http::api_router(state, auth);
    let mut unix_server = tokio::spawn(async move { axum::serve(unix, router).await });
    println!(
        "{}",
        serde_json::json!({"origin": origin, "setup": std::fs::read_to_string(setup)?.trim(), "socket": socket})
    );
    std::io::Write::flush(&mut std::io::stdout())?;
    // The parent closes stdin in fixture teardown, including after a failed test.
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let result = std::io::Read::read_to_end(&mut std::io::stdin(), &mut Vec::new());
        let _ = shutdown.send(result);
    });
    let result = tokio::select! {
        result = &mut server => result?.map_err(anyhow::Error::from),
        result = &mut unix_server => result?.map_err(anyhow::Error::from),
        result = &mut controller => result?,
        result = stopped => result?.map(|_| ()).map_err(anyhow::Error::from),
    };
    server.abort();
    unix_server.abort();
    controller.abort();
    result
}
