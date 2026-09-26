//! Isolated UI/API fixture for Playwright. Docker execution is deliberately stubbed.
use async_trait::async_trait;
use piqueld::{
    api::http::{ApiState, UiAssets},
    application::{BoundaryError, RuntimeBoundary},
    store::{Store, StoredApplication},
};
use piqueld_core::{
    ApplicationId, NormalizedApplication, ObservedApplication, ResolutionSet,
    resource::ResolvedApplication,
};
use std::sync::Arc;
struct Runtime;
#[async_trait]
impl RuntimeBoundary for Runtime {
    async fn logs(
        &self,
        _: &ApplicationId,
        _: Option<&str>,
        _: u16,
        _: u32,
        _: Option<piqueld_core::api::LogStream>,
    ) -> Result<piqueld_core::api::ApplicationLogs, BoundaryError> {
        Ok(piqueld_core::api::ApplicationLogs::default())
    }
    async fn prepare(
        &self,
        _: &NormalizedApplication,
        _: &ResolutionSet,
    ) -> Result<ResolvedApplication, BoundaryError> {
        Err(piqueld::docker::DockerError::Unavailable("browser fixture").into())
    }
    async fn check_available(&self) -> Result<(), BoundaryError> {
        Ok(())
    }
    async fn observe(&self, _: &StoredApplication) -> Result<ObservedApplication, BoundaryError> {
        Ok(ObservedApplication::default())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = std::path::PathBuf::from(std::env::var("PIQUELD_E2E_DATA_DIR")?);
    let store = Arc::new(Store::open(dir.join("db")).await?);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("http://localhost:{}", listener.local_addr()?.port());
    let auth = piqueld::auth::Auth::new(&store, &origin)?;
    let setup = dir.join("setup-link");
    auth.prepare_setup(&setup).await?;
    let state = ApiState::new(store, Arc::new(Runtime));
    let router = piqueld::api::http::protect(
        piqueld::api::http::web_router(state.clone(), UiAssets::resolve()),
        auth.clone(),
    );
    let mut server = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
    });
    let socket = dir.join("api.sock");
    let unix = tokio::net::UnixListener::bind(&socket)?;
    let router = piqueld::api::http::protect(piqueld::api::http::api_router(state), auth);
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
