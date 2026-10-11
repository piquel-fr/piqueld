//! Isolated UI/API fixture for Playwright. Docker execution is deliberately
//! stubbed; commands in service `web` run a scripted terminal instead.
use async_trait::async_trait;
use piqueld::{
    api::http::{ApiState, UiAssets},
    application::{BoundaryError, RuntimeBoundary},
    docker::{DockerError, Exec, ExecIo},
    store::{Store, StoredEnvironment},
};
use piqueld_core::{
    EnvironmentId, NormalizedApplication, ObservedApplication, ResolutionSet,
    exec::{ExecInput, ExecOutput, ExecRequest},
    resource::ResolvedApplication,
};
use std::sync::Arc;
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
    async fn readiness(&self) -> (bool, bool) {
        (false, false)
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = std::path::PathBuf::from(std::env::var("PIQUELD_PLAYWRIGHT_DATA_DIR")?);
    let store = Arc::new(Store::open(dir.join("db")).await?);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("http://localhost:{}", listener.local_addr()?.port());
    let auth = piqueld::auth::Auth::new(&store, &origin)?;
    let setup = dir.join("setup-link");
    auth.prepare_setup(&setup).await?;
    let state = ApiState::new(store, Arc::new(Runtime));
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
        result = &mut server => result?,
        result = &mut unix_server => result?,
        result = stopped => result?.map(|_| ()),
    };
    server.abort();
    unix_server.abort();
    result?;
    Ok(())
}
