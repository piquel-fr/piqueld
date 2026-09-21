//! Real browser `WebAuthn` integration, using Chromium's virtual authenticator.
//! Run with CHROMIUM and CHROMEDRIVER pointing to executables and embedded-ui enabled.
#![cfg(feature = "embedded-ui")]
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
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Chromium, chromedriver, Python, and the embedded UI"]
async fn passkey_browser_and_cli_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("db")).await.unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://localhost:{}", listener.local_addr().unwrap().port());
    let auth = piqueld::auth::Auth::new(&store, &origin).unwrap();
    let setup = dir.path().join("setup-link");
    auth.prepare_setup(&setup).await.unwrap();
    let state = ApiState::new(store, Arc::new(Runtime));
    let router = piqueld::api::http::protect(
        piqueld::api::http::web_router(state.clone(), UiAssets::resolve()),
        auth.clone(),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let socket = dir.path().join("api.sock");
    let unix = tokio::net::UnixListener::bind(&socket).unwrap();
    let router = piqueld::api::http::protect(piqueld::api::http::api_router(state), auth);
    let unix_server = tokio::spawn(async move {
        axum::serve(unix, router).await.unwrap();
    });
    let script =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/test-auth-browser.py");
    let result = tokio::task::spawn_blocking(move || {
        std::process::Command::new("python3")
            .arg(script)
            .env("AUTH_TEST_ORIGIN", origin)
            .env("AUTH_TEST_SETUP", setup)
            .env("AUTH_TEST_SOCKET", socket)
            .env("AUTH_TEST_DIR", dir.path())
            .status()
            .unwrap()
    })
    .await
    .unwrap();
    server.abort();
    unix_server.abort();
    assert!(
        result.success(),
        "browser authentication integration failed"
    );
}
