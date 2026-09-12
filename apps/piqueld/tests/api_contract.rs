//! Focused API/client coverage for the polling application lifecycle.

use async_trait::async_trait;
use axum::{body::Body, http::Request, serve};
use http_body_util::BodyExt;
use piqueld::api::{ApiState, EmbeddedBundle, UiAssets, api_router, router, web_router};
use piqueld::application::{BoundaryError, RuntimeBoundary};
use piqueld::store::{SqliteStore, StoredApplication};
use piqueld_client::{AcceptedOperation, ApplyApplicationRequest, Client};
use piqueld_core::{
    InstanceId, NormalizedApplication, ObservedApplication, ResolutionSet, compile_application,
    manifest::{ApplicationManifest, Source},
    planner::ActionKind,
    resource::ResolvedSource,
};
use std::{collections::BTreeMap, future::IntoFuture, sync::Arc};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tower::ServiceExt;

struct FakeRuntime {
    instance: InstanceId,
    unavailable: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl RuntimeBoundary for FakeRuntime {
    async fn prepare(
        &self,
        application: &NormalizedApplication,
        _reusable: &piqueld_core::ResolutionSet,
    ) -> Result<piqueld_core::ResolvedApplication, BoundaryError> {
        let sources = application
            .spec
            .services
            .iter()
            .map(|service| {
                let Source::Image { image } = &service.source else {
                    panic!("expected image fixture")
                };
                let repository = image
                    .rsplit_once(':')
                    .map_or(image.as_str(), |value| value.0);
                (
                    service.name.clone(),
                    ResolvedSource::Image {
                        requested: image.clone(),
                        digest_reference: format!("{repository}@sha256:{}", "a".repeat(64)),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let resolved = compile_application(
            application,
            self.instance.clone(),
            &ResolutionSet { sources },
        )
        .map_err(BoundaryError::Compilation)?;
        Ok(resolved)
    }

    async fn check_available(&self) -> Result<(), BoundaryError> {
        if self.unavailable.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(piqueld::docker::DockerError::Unavailable("observe application").into());
        }
        Ok(())
    }

    async fn observe(
        &self,
        _application: &StoredApplication,
    ) -> Result<ObservedApplication, BoundaryError> {
        self.check_available().await?;
        Ok(ObservedApplication::default())
    }
}

fn manifest() -> ApplicationManifest {
    serde_json::from_value(serde_json::json!({
        "api_version": "piqueld.dev/v1alpha1",
        "kind": "Application",
        "metadata": {"name": "notes"},
        "spec": {"services": [{
            "name": "web",
            "source": {"type": "image", "image": "ghcr.io/example/notes:1"}
        }]}
    }))
    .expect("fixture is valid")
}

async fn state(temp: &TempDir) -> ApiState {
    let store = Arc::new(
        SqliteStore::open(temp.path().join("state.db"))
            .await
            .expect("fresh database opens"),
    );
    let instance = InstanceId::parse(store.instance_id().to_owned()).expect("valid instance ID");
    ApiState::new(
        Arc::clone(&store),
        Arc::new(FakeRuntime {
            instance,
            unavailable: std::sync::atomic::AtomicBool::new(false),
        }),
    )
}

/// Stand-in for the compile-time bundle: a shell, unhashed assets (including
/// a short all-hex stem that must not read as a digest), and a content-hashed
/// asset, mirroring what Trunk emits.
static TEST_BUNDLE: &EmbeddedBundle = &[
    ("added.css", b"body{}" as &'static [u8]),
    ("app.js", b"console.log('dashboard');" as &'static [u8]),
    (
        "index.html",
        b"<!doctype html><html><body><main>dashboard-shell</main></body></html>" as &'static [u8],
    ),
    ("piqueld-ui-a1b2c3d4_bg.wasm", b"\0asm" as &'static [u8]),
];

#[tokio::test]
async fn typed_client_exercises_polling_lifecycle_over_tcp() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("TCP listener binds");
    let address = listener.local_addr().expect("listener address is readable");
    let server = tokio::spawn(serve(listener, router(state(&temp).await)).into_future());
    let client = Client::tcp(&format!("http://{address}/")).expect("valid client endpoint");
    let manifest = manifest();

    assert_create_plan(&client, &manifest).await;
    let created = create_and_inspect(&client, &manifest).await;
    let replaced = replace_and_plan(&client, &created, manifest).await;
    assert_ne!(created.operation_id, replaced.operation_id);
    delete(&client, &replaced).await;

    server.abort();
}

async fn assert_create_plan(client: &Client, manifest: &ApplicationManifest) {
    let preview = client
        .plan_application(&ApplyApplicationRequest {
            expected_generation: None,
            expected_application_id: None,
            manifest: manifest.clone(),
        })
        .await
        .expect("create preview succeeds");
    assert!(matches!(
        preview.plan.actions.as_slice(),
        [action] if matches!(action.kind, ActionKind::ResolveImage { .. })
    ));
}

async fn create_and_inspect(client: &Client, manifest: &ApplicationManifest) -> AcceptedOperation {
    let mut request = ApplyApplicationRequest {
        expected_generation: Some(0),
        expected_application_id: None,
        manifest: manifest.clone(),
    };
    let created = client
        .apply_application(&request)
        .await
        .expect("apply succeeds");
    request.expected_generation = Some(created.generation);
    request.expected_application_id = Some(created.application_id.clone());
    let replay = client
        .apply_application(&request)
        .await
        .expect("apply retry succeeds");
    assert_eq!(created.operation_id, replay.operation_id);
    assert_eq!(client.applications().await.unwrap().items.len(), 1);
    let detail = client
        .application_detail(&created.application_id)
        .await
        .expect("detail succeeds");
    assert_eq!(
        detail.application.application.id,
        created.application_id.parse().unwrap()
    );
    assert_eq!(detail.application.application.spec.services.len(), 1);
    assert!(detail.observed.services.is_empty());
    assert_eq!(detail.application.resolved_generation, None);
    assert!(detail.diagnostics.is_empty());
    assert_eq!(
        client.operation(&created.operation_id).await.unwrap().kind,
        piqueld_core::OperationKind::Apply
    );
    created
}

#[tokio::test]
async fn dashboard_fallback_preserves_api_and_asset_route_precedence() {
    let temp = tempfile::tempdir().expect("temporary directory");

    let application = web_router(state(&temp).await, UiAssets::Embedded(TEST_BUNDLE));
    assert_dashboard_routes(&application).await;
    assert_api_routes(&application).await;
    assert_api_only_and_ui_modes(&temp).await;
}

async fn assert_dashboard_routes(application: &axum::Router) {
    let root = response_text(
        application
            .clone()
            .oneshot(request("/"))
            .await
            .expect("root request succeeds"),
    )
    .await;
    assert_eq!(root.0, axum::http::StatusCode::PERMANENT_REDIRECT);
    assert_eq!(root.2.as_deref(), Some("/dashboard/"));

    let dashboard_root = response_text(
        application
            .clone()
            .oneshot(request("/dashboard"))
            .await
            .expect("dashboard root request succeeds"),
    )
    .await;
    assert_eq!(dashboard_root.0, axum::http::StatusCode::PERMANENT_REDIRECT);
    assert_eq!(dashboard_root.2.as_deref(), Some("/dashboard/"));

    let dashboard = response_text(
        application
            .clone()
            .oneshot(request("/dashboard/"))
            .await
            .expect("dashboard request succeeds"),
    )
    .await;
    assert_eq!(dashboard.0, axum::http::StatusCode::OK);
    assert!(dashboard.1.contains("dashboard-shell"));

    let deep_link = response_text(
        application
            .clone()
            .oneshot(request("/dashboard/applications/notes"))
            .await
            .expect("deep link request succeeds"),
    )
    .await;
    assert_eq!(deep_link.0, axum::http::StatusCode::OK);
    assert!(deep_link.1.contains("dashboard-shell"));

    let asset = response_text(
        application
            .clone()
            .oneshot(request("/dashboard/app.js"))
            .await
            .expect("asset request succeeds"),
    )
    .await;
    assert_eq!(asset.0, axum::http::StatusCode::OK);
    assert!(asset.1.contains("console.log"));

    let hashed = response_text(
        application
            .clone()
            .oneshot(request("/dashboard/piqueld-ui-a1b2c3d4_bg.wasm"))
            .await
            .expect("hashed asset request succeeds"),
    )
    .await;
    assert_eq!(hashed.0, axum::http::StatusCode::OK);
    assert!(hashed.1.starts_with('\u{0}'));

    let missing_asset = response_text(
        application
            .clone()
            .oneshot(request("/dashboard/missing.js"))
            .await
            .expect("missing asset request succeeds"),
    )
    .await;
    assert_eq!(missing_asset.0, axum::http::StatusCode::NOT_FOUND);
    assert!(!missing_asset.1.contains("dashboard-shell"));

    let api_error = response_text(
        application
            .clone()
            .oneshot(request("/api/v1/unknown"))
            .await
            .expect("unknown API request succeeds"),
    )
    .await;
    assert_eq!(api_error.0, axum::http::StatusCode::NOT_FOUND);
    assert!(api_error.1.contains("endpoint_not_found"));
    assert!(!api_error.1.contains("dashboard-shell"));

    let outside = response_text(
        application
            .clone()
            .oneshot(request("/unknown"))
            .await
            .expect("unknown web request succeeds"),
    )
    .await;
    assert_eq!(outside.0, axum::http::StatusCode::NOT_FOUND);
    assert!(!outside.1.contains("dashboard-shell"));
}

async fn assert_api_routes(application: &axum::Router) {
    let health = response_text(
        application
            .clone()
            .oneshot(request("/health"))
            .await
            .expect("health request succeeds"),
    )
    .await;
    assert_eq!(health.0, axum::http::StatusCode::OK);
    assert_eq!(health.1, r#"{"status":"ok"}"#);

    let openapi = response_text(
        application
            .clone()
            .oneshot(request("/api/v1/openapi.json"))
            .await
            .expect("OpenAPI request succeeds"),
    )
    .await;
    assert_eq!(openapi.0, axum::http::StatusCode::OK);
    assert!(openapi.1.contains("/api/v1/applications/{id}/detail"));
}

async fn assert_api_only_and_ui_modes(temp: &TempDir) {
    let api_only = api_router(state(temp).await);
    let unix_root = response_text(
        api_only
            .clone()
            .oneshot(request("/"))
            .await
            .expect("API-only root request succeeds"),
    )
    .await;
    assert_eq!(unix_root.0, axum::http::StatusCode::NOT_FOUND);
    assert!(!unix_root.1.contains("endpoint_not_found"));

    let unix_health = response_text(
        api_only
            .clone()
            .oneshot(request("/health"))
            .await
            .expect("API-only health request succeeds"),
    )
    .await;
    assert_eq!(unix_health.0, axum::http::StatusCode::NOT_FOUND);

    let api_only_error = response_text(
        api_only
            .oneshot(request("/api/v1/unknown"))
            .await
            .expect("API-only unknown request succeeds"),
    )
    .await;
    assert_eq!(api_only_error.0, axum::http::StatusCode::NOT_FOUND);
    assert!(api_only_error.1.contains("endpoint_not_found"));

    let disabled = web_router(state(temp).await, UiAssets::Disabled);
    let disabled_root = response_text(
        disabled
            .clone()
            .oneshot(request("/"))
            .await
            .expect("disabled UI root request succeeds"),
    )
    .await;
    assert_eq!(disabled_root.0, axum::http::StatusCode::NOT_FOUND);
    let disabled_dashboard = response_text(
        disabled
            .oneshot(request("/dashboard/"))
            .await
            .expect("disabled UI dashboard request succeeds"),
    )
    .await;
    assert_eq!(disabled_dashboard.0, axum::http::StatusCode::NOT_FOUND);
}

fn request(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("request is valid")
}

#[tokio::test]
async fn dashboard_responses_carry_security_headers() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let application = web_router(state(&temp).await, UiAssets::Embedded(TEST_BUNDLE));
    let shell = application
        .clone()
        .oneshot(request("/dashboard/"))
        .await
        .expect("dashboard request succeeds");
    assert_eq!(shell.status(), axum::http::StatusCode::OK);
    let headers = shell.headers().clone();
    let csp = headers
        .get("content-security-policy")
        .and_then(|value| value.to_str().ok())
        .expect("dashboard carries a content security policy");
    assert!(csp.contains("default-src 'self'"), "{csp}");
    assert!(csp.contains("wasm-unsafe-eval"), "{csp}");
    assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    assert_eq!(
        headers
            .get("x-content-type-options")
            .and_then(|value| value.to_str().ok()),
        Some("nosniff")
    );
    assert_eq!(
        headers
            .get("referrer-policy")
            .and_then(|value| value.to_str().ok()),
        Some("no-referrer")
    );

    let head = application
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::HEAD)
                .uri("/dashboard/")
                .body(Body::empty())
                .expect("HEAD request is valid"),
        )
        .await
        .expect("dashboard HEAD succeeds");
    assert_eq!(head.status(), StatusCode::OK);
    assert!(
        head.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty()
    );

    let method_not_allowed = application
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/dashboard/")
                .body(Body::empty())
                .expect("POST request is valid"),
        )
        .await
        .expect("dashboard POST completes");
    assert_eq!(method_not_allowed.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        method_not_allowed.headers().get(http::header::ALLOW),
        Some(&HeaderValue::from_static("GET, HEAD"))
    );
}

#[tokio::test]
async fn dashboard_cache_policy_tracks_content_hashing_and_shell_fallbacks() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let application = web_router(state(&temp).await, UiAssets::Embedded(TEST_BUNDLE));

    let hashed = application
        .clone()
        .oneshot(request("/dashboard/piqueld-ui-a1b2c3d4_bg.wasm"))
        .await
        .expect("hashed asset request succeeds");
    assert_eq!(
        header_value(&hashed, "content-type"),
        Some("application/wasm")
    );
    assert_eq!(
        header_value(&hashed, "cache-control"),
        Some("public, max-age=31536000, immutable")
    );

    let unhashed = application
        .clone()
        .oneshot(request("/dashboard/app.js"))
        .await
        .expect("unhashed asset request succeeds");
    assert_eq!(
        header_value(&unhashed, "content-type"),
        Some("text/javascript; charset=utf-8")
    );
    assert_eq!(header_value(&unhashed, "cache-control"), Some("no-cache"));

    // A short all-hex stem is an ordinary name, not a Trunk digest.
    let hex_stemmed = application
        .clone()
        .oneshot(request("/dashboard/added.css"))
        .await
        .expect("hex-stemmed asset request succeeds");
    assert_eq!(
        header_value(&hex_stemmed, "cache-control"),
        Some("no-cache")
    );

    for path in ["/dashboard/", "/dashboard/applications/notes"] {
        let shell = response_text(
            application
                .clone()
                .oneshot(request(path))
                .await
                .unwrap_or_else(|_| panic!("{path} succeeds")),
        )
        .await;
        assert_eq!(shell.0, axum::http::StatusCode::OK, "{path}");
        assert!(shell.1.contains("dashboard-shell"), "{path}");
    }

    let shell_headers = application
        .clone()
        .oneshot(request("/dashboard/"))
        .await
        .expect("shell request succeeds");
    assert_eq!(
        header_value(&shell_headers, "cache-control"),
        Some("no-store")
    );
}

fn header_value<'a>(response: &'a axum::response::Response, name: &str) -> Option<&'a str> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
}

async fn response_text(
    response: axum::response::Response,
) -> (axum::http::StatusCode, String, Option<String>) {
    let status = response.status();
    let location = response
        .headers()
        .get(axum::http::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("response body is readable")
        .to_bytes();
    (
        status,
        String::from_utf8(body.into_iter().collect()).expect("response is UTF-8"),
        location,
    )
}

async fn replace_and_plan(
    client: &Client,
    created: &AcceptedOperation,
    mut manifest: ApplicationManifest,
) -> AcceptedOperation {
    manifest.spec.services[0].replicas = 2;
    let request = ApplyApplicationRequest {
        expected_generation: Some(created.generation),
        expected_application_id: Some(created.application_id.clone()),
        manifest,
    };
    let replaced = client
        .apply_application(&request)
        .await
        .expect("replacement succeeds");
    assert_eq!(replaced.application_id, created.application_id);
    let mut preview_request = request;
    preview_request.expected_generation = Some(replaced.generation);
    client
        .plan_application(&preview_request)
        .await
        .expect("preview succeeds");
    replaced
}

async fn delete(client: &Client, created: &AcceptedOperation) {
    let deleted = client
        .delete_application_with_generation(&created.application_id, Some(created.generation))
        .await
        .expect("delete succeeds");
    assert_eq!(
        client.operation(&deleted.operation_id).await.unwrap().kind,
        piqueld_core::OperationKind::Delete
    );
}

// ---------------------------------------------------------------------------
// Raw-transport helpers and negative-path coverage restored from the former
// in-crate API suite.
// ---------------------------------------------------------------------------

use http::{HeaderMap, HeaderValue, Method, Request as HttpRequest, StatusCode, Uri};
use http_body_util::Full;
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;

struct RawResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: serde_json::Value,
}

impl RawResponse {
    fn code(&self) -> &str {
        self.body
            .get("code")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
    }

    fn request_id(&self) -> Option<&str> {
        self.body
            .get("request_id")
            .and_then(serde_json::Value::as_str)
    }
}

async fn send_raw(
    target: Target<'_>,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> RawResponse {
    let uri = Uri::builder().path_and_query(path).build().expect("uri");
    let mut builder = HttpRequest::builder()
        .method(method)
        .uri(uri)
        .header(http::header::CONTENT_LENGTH, body.len());
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .body(Full::new(Bytes::from(body)))
        .expect("request builds");
    let response = match target {
        Target::Tcp(address) => {
            let stream = tokio::net::TcpStream::connect(address)
                .await
                .expect("tcp connects");
            let (mut sender, connection) =
                hyper::client::conn::http1::handshake(TokioIo::new(stream))
                    .await
                    .expect("handshake");
            tokio::spawn(async move {
                let _ = connection.with_upgrades().await;
            });
            sender.send_request(request).await.expect("response")
        }
        Target::Unix(path) => {
            let stream = tokio::net::UnixStream::connect(path)
                .await
                .expect("unix connects");
            let (mut sender, connection) =
                hyper::client::conn::http1::handshake(TokioIo::new(stream))
                    .await
                    .expect("handshake");
            tokio::spawn(async move {
                let _ = connection.with_upgrades().await;
            });
            sender.send_request(request).await.expect("response")
        }
    };
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = BodyExt::collect(response.into_body())
        .await
        .expect("body collects")
        .to_bytes();
    let body = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    RawResponse {
        status,
        headers,
        body,
    }
}

enum Target<'a> {
    Tcp(std::net::SocketAddr),
    #[allow(dead_code)]
    Unix(&'a std::path::Path),
}

#[tokio::test]
async fn transport_failures_are_structured_safe_and_request_ids_pair() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(serve(listener, router(state(&temp).await)).into_future());

    let huge = format!(
        "{{\"manifest\": {{\"padding\": \"{}\"}}}}",
        "x".repeat(3 * 1024 * 1024)
    );
    let too_large = send_raw(
        Target::Tcp(address),
        Method::POST,
        "/api/v1/applications/apply",
        &[("content-type", "application/json")],
        huge.into_bytes(),
    )
    .await;
    assert_eq!(too_large.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(too_large.code(), "request_body_too_large");

    let missing = send_raw(
        Target::Tcp(address),
        Method::GET,
        "/api/v1/does-not-exist",
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.code(), "endpoint_not_found");

    let not_allowed = send_raw(
        Target::Tcp(address),
        Method::PUT,
        "/api/v1/applications",
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(not_allowed.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(not_allowed.code(), "method_not_allowed");

    let bad_cursor = send_raw(
        Target::Tcp(address),
        Method::GET,
        "/api/v1/applications?cursor=bogus",
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(bad_cursor.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad_cursor.code(), "pagination_invalid");

    let bad_limit = send_raw(
        Target::Tcp(address),
        Method::GET,
        "/api/v1/applications?limit=0",
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(bad_limit.status, StatusCode::BAD_REQUEST);

    let malformed = send_raw(
        Target::Tcp(address),
        Method::POST,
        "/api/v1/applications/plan",
        &[("content-type", "application/json")],
        b"{\"manifest\": {\"broken\"".to_vec(),
    )
    .await;
    assert_eq!(malformed.status, StatusCode::BAD_REQUEST);
    assert_eq!(malformed.code(), "json_malformed");

    let paired = send_raw(
        Target::Tcp(address),
        Method::GET,
        "/api/v1/applications/doesnotexist1",
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(paired.status, StatusCode::NOT_FOUND);
    let header_id = paired
        .headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .expect("request id header");
    assert_eq!(paired.request_id(), Some(header_id));

    server.abort();
}

#[tokio::test]
async fn method_not_allowed_advertises_only_the_matched_route_methods() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(serve(listener, router(state(&temp).await)).into_future());

    // The Allow header advertises only the methods registered for the matched
    // route; Axum serves HEAD automatically for every GET route.
    let collection = send_raw(
        Target::Tcp(address),
        Method::PUT,
        "/api/v1/applications",
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(
        collection.headers.get(http::header::ALLOW),
        Some(&HeaderValue::from_static("GET, HEAD"))
    );

    let by_id = send_raw(
        Target::Tcp(address),
        Method::POST,
        "/api/v1/applications/app-notes-01",
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(by_id.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        by_id.headers.get(http::header::ALLOW),
        Some(&HeaderValue::from_static("GET, HEAD, DELETE"))
    );

    let health = send_raw(
        Target::Tcp(address),
        Method::POST,
        "/health",
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(health.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        health.headers.get(http::header::ALLOW),
        Some(&HeaderValue::from_static("GET, HEAD"))
    );

    server.abort();
}

#[tokio::test]
async fn manifest_validation_media_types_and_unknown_fields_are_rejected_safely() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(serve(listener, router(state(&temp).await)).into_future());

    let invalid_manifest = r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "notes"
[[spec.services]]
name = "web"
[spec.services.source]
type = "image"
image = ""
"#;
    let validation = send_raw(
        Target::Tcp(address),
        Method::POST,
        "/api/v1/applications/plan",
        &[("content-type", "application/toml")],
        invalid_manifest.as_bytes().to_vec(),
    )
    .await;
    assert_eq!(validation.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(validation.code(), "manifest_validation_failed");

    let malformed_toml = send_raw(
        Target::Tcp(address),
        Method::POST,
        "/api/v1/applications/plan",
        &[("content-type", "text/toml")],
        b"api_version = ".to_vec(),
    )
    .await;
    assert_eq!(malformed_toml.status, StatusCode::BAD_REQUEST);
    assert_eq!(malformed_toml.code(), "toml_malformed");

    let unknown_field = serde_json::json!({
        "manifest": manifest(),
        "surprise": true
    });
    let unknown = send_raw(
        Target::Tcp(address),
        Method::POST,
        "/api/v1/applications/apply",
        &[("content-type", "application/json")],
        serde_json::to_vec(&unknown_field).expect("serializes"),
    )
    .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    assert_eq!(unknown.code(), "json_malformed");

    // TOML creation shares the JSON normalization pipeline.
    let valid_toml = r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "tomlnotes"
[[spec.services]]
name = "web"
replicas = 2
[spec.services.source]
type = "image"
image = "ghcr.io/example/notes:1"
"#;
    let created = send_raw(
        Target::Tcp(address),
        Method::POST,
        "/api/v1/applications/apply",
        &[
            ("content-type", "application/toml"),
            ("x-expected-generation", "0"),
        ],
        valid_toml.as_bytes().to_vec(),
    )
    .await;
    assert_eq!(created.status, StatusCode::ACCEPTED);
    assert!(created.body["data"]["operation_id"].is_string());

    server.abort();
}

#[tokio::test]
async fn accepts_foreign_authorities() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(serve(listener, router(state(&temp).await)).into_future());

    let rebinding = send_raw(
        Target::Tcp(address),
        Method::GET,
        "/api/v1/system/status",
        &[("host", "attacker.example")],
        Vec::new(),
    )
    .await;
    assert_eq!(rebinding.status, StatusCode::OK);
    let header_id = rebinding
        .headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok());
    assert!(header_id.is_some());

    let loopback = send_raw(
        Target::Tcp(address),
        Method::GET,
        "/api/v1/system/status",
        &[("host", &format!("127.0.0.1:{port}", port = address.port()))],
        Vec::new(),
    )
    .await;
    assert_eq!(loopback.status, StatusCode::OK);

    let loopback_alias = send_raw(
        Target::Tcp(address),
        Method::GET,
        "/api/v1/system/status",
        &[("host", "127.0.0.2:7845")],
        Vec::new(),
    )
    .await;
    assert_eq!(loopback_alias.status, StatusCode::OK);

    let ipv6 = send_raw(
        Target::Tcp(address),
        Method::GET,
        "/api/v1/system/status",
        &[("host", "[::1]:9999")],
        Vec::new(),
    )
    .await;
    assert_eq!(ipv6.status, StatusCode::OK);

    let malformed_ipv6 = send_raw(
        Target::Tcp(address),
        Method::GET,
        "/api/v1/system/status",
        &[("host", "[::1]attacker.example")],
        Vec::new(),
    )
    .await;
    assert_eq!(malformed_ipv6.status, StatusCode::OK);

    // A bare unbracketed IPv6 literal has no port to split off; its colons
    // must not be mistaken for a host:port separator.
    let bare_ipv6 = send_raw(
        Target::Tcp(address),
        Method::GET,
        "/api/v1/system/status",
        &[("host", "::1")],
        Vec::new(),
    )
    .await;
    assert_eq!(bare_ipv6.status, StatusCode::OK);

    let foreign_ipv6 = send_raw(
        Target::Tcp(address),
        Method::GET,
        "/api/v1/system/status",
        &[("host", "fe80::1")],
        Vec::new(),
    )
    .await;
    assert_eq!(foreign_ipv6.status, StatusCode::OK);

    server.abort();
}

#[tokio::test]
async fn served_openapi_document_matches_the_generated_snapshot_and_resolves_refs() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(serve(listener, router(state(&temp).await)).into_future());

    let document_response = send_raw(
        Target::Tcp(address),
        Method::GET,
        "/api/v1/openapi.json",
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(document_response.status, StatusCode::OK);
    let generated =
        serde_json::to_value(piqueld::api::openapi_document()).expect("document serializes");
    assert_eq!(document_response.body, generated);

    let text = serde_json::to_string(&generated).expect("document stringifies");
    let mut unresolved = Vec::new();
    collect_unresolved_refs(&generated, &text, &mut unresolved);
    assert!(unresolved.is_empty(), "unresolved refs: {unresolved:?}");

    server.abort();
}

fn collect_unresolved_refs(value: &serde_json::Value, document: &str, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(reference) = map.get("$ref").and_then(serde_json::Value::as_str) {
                let pointer = reference.strip_prefix("#").unwrap_or(reference);
                let resolved = document
                    .parse::<serde_json::Value>()
                    .is_ok_and(|doc| doc.pointer(pointer).is_some());
                if !resolved {
                    out.push(reference.to_owned());
                }
            }
            for child in map.values() {
                collect_unresolved_refs(child, document, out);
            }
        }
        serde_json::Value::Array(items) => {
            for child in items {
                collect_unresolved_refs(child, document, out);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn typed_client_exercises_the_lifecycle_over_a_unix_socket() {
    let temp = tempfile::tempdir_in(".").expect("temporary directory");
    let data_dir = temp.path().join("state");
    std::fs::create_dir(&data_dir).expect("data dir exists");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700))
            .expect("data dir is private");
    }
    // The fixture is owned by this process; Nix's synthetic root is not.
    let cwd = std::env::current_dir().expect("working directory");
    piqueld::prepare_data_dir(data_dir.strip_prefix(&cwd).expect("fixture is below cwd"))
        .await
        .expect("data dir prepares");
    let socket_path = data_dir.join("contract.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).expect("unix binds");
    let server = tokio::spawn(serve(listener, router(state(&temp).await)).into_future());
    let client = Client::unix(&socket_path);

    let status = client.system_status().await.expect("status over unix");
    assert_eq!(status.api_version, "v1");

    assert_create_plan(&client, &manifest()).await;
    let created = create_and_inspect(&client, &manifest()).await;
    let replaced = replace_and_plan(&client, &created, manifest()).await;
    assert_ne!(created.operation_id, replaced.operation_id);
    delete(&client, &replaced).await;

    server.abort();
}

#[tokio::test]
async fn generations_refresh_reconcile_and_event_pagination_share_the_http_contract() {
    let temp = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(serve(listener, router(state(&temp).await)).into_future());
    let client = Client::tcp(&format!("http://{address}")).unwrap();
    let mut request = ApplyApplicationRequest {
        manifest: manifest(),
        expected_generation: Some(0),
        expected_application_id: None,
    };
    let first = client.apply_application(&request).await.unwrap();
    assert_eq!(first.generation, 1);
    let stale = client.apply_application(&request).await.unwrap_err();
    assert!(
        matches!(stale,piqueld_client::ClientError::Api {status,..} if status==axum::http::StatusCode::CONFLICT)
    );
    request.expected_generation = Some(1);
    request.expected_application_id = Some(first.application_id.clone());
    request.manifest.spec.services[0].replicas = 2;
    let changed = client.apply_application(&request).await.unwrap();
    assert_eq!(changed.generation, 2);
    let refreshed = client
        .refresh_application(&first.application_id, Some(2))
        .await
        .unwrap();
    assert_eq!(refreshed.generation, 2);
    assert_ne!(refreshed.operation_id, changed.operation_id);
    let reconciled = client
        .reconcile_application(&first.application_id, Some(2))
        .await
        .unwrap();
    assert_eq!(reconciled.operation_id, refreshed.operation_id);
    let first_page = client
        .events(Some(&first.application_id), None, 2)
        .await
        .unwrap();
    assert_eq!(first_page.items.len(), 2);
    let second_page = client
        .events(
            Some(&first.application_id),
            first_page.next_cursor.as_deref(),
            100,
        )
        .await
        .unwrap();
    assert!(!second_page.items.is_empty());
    assert!(
        second_page
            .items
            .iter()
            .all(|event| event.id > first_page.items.last().unwrap().id)
    );
    let deletion = client
        .delete_application_with_generation(&first.application_id, Some(2))
        .await
        .unwrap();
    assert_eq!(deletion.generation, 3);
    assert!(
        client
            .refresh_application(&first.application_id, None)
            .await
            .is_err()
    );
    let repeated = client
        .delete_application_with_generation(&first.application_id, Some(deletion.generation))
        .await
        .unwrap();
    assert_eq!(repeated.operation_id, deletion.operation_id);
    task.abort();
}

struct AcceptanceApi {
    client: Client,
    runtime: Arc<FakeRuntime>,
    store: Arc<SqliteStore>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl AcceptanceApi {
    async fn start(temp: &TempDir) -> Self {
        let store = Arc::new(
            SqliteStore::open(temp.path().join("state.db"))
                .await
                .unwrap(),
        );
        let instance = InstanceId::parse(store.instance_id()).unwrap();
        let runtime = Arc::new(FakeRuntime {
            instance,
            unavailable: std::sync::atomic::AtomicBool::new(false),
        });
        let state = ApiState::new(Arc::clone(&store), runtime.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::tcp(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let task = tokio::spawn(serve(listener, router(state)).into_future());
        Self {
            client,
            runtime,
            store,
            task,
        }
    }

    fn request() -> ApplyApplicationRequest {
        ApplyApplicationRequest {
            manifest: manifest(),
            expected_generation: Some(0),
            expected_application_id: None,
        }
    }
}

impl Drop for AcceptanceApi {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn acceptance_receipts_survive_restart_and_supersession() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let request = AcceptanceApi::request();
    let keyed = api.client.clone().with_request_id("apply-command-1");
    let accepted = keyed.apply_application(&request).await.unwrap();
    let mut replacement = request.clone();
    replacement.expected_generation = Some(1);
    replacement.expected_application_id = Some(accepted.application_id.clone());
    replacement.manifest.spec.services[0].replicas = 2;
    api.client.apply_application(&replacement).await.unwrap();
    drop(api);
    let api = AcceptanceApi::start(&temp).await;
    let keyed = api.client.clone().with_request_id("apply-command-1");
    let replayed = keyed.apply_application(&request).await.unwrap();
    assert_eq!(replayed.operation_id, accepted.operation_id);
    assert_eq!(replayed.generation, 1);
    assert_eq!(
        api.client
            .application(&accepted.application_id)
            .await
            .unwrap()
            .generation,
        2
    );
    assert_eq!(
        api.client
            .operation(&accepted.operation_id)
            .await
            .unwrap()
            .state,
        piqueld_core::OperationState::Superseded
    );
    let error = keyed.apply_application(&replacement).await.unwrap_err();
    assert!(
        matches!(error,piqueld_client::ClientError::Api {error,..} if error.code=="request_id_conflict")
    );
    // Two concurrent requests still produce exactly one durable acceptance.
    let mut fresh = request.clone();
    fresh.manifest.metadata.name = "another".into();
    let concurrent = api.client.clone().with_request_id("concurrent-command");
    let (a, b) = tokio::join!(
        concurrent.apply_application(&fresh),
        concurrent.apply_application(&fresh)
    );
    assert_eq!(a.unwrap().operation_id, b.unwrap().operation_id);
}

#[tokio::test]
async fn rename_is_conditioned_idle_only_and_replayable_without_deployment() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let accepted = api
        .client
        .apply_application(&AcceptanceApi::request())
        .await
        .unwrap();
    let request = piqueld_client::RenameApplicationRequest {
        name: "renamed".into(),
        expected_generation: Some(1),
    };
    let keyed = api.client.clone().with_request_id("rename-command");
    let error = keyed
        .rename_application(&accepted.application_id, &request)
        .await
        .unwrap_err();
    assert!(
        matches!(error,piqueld_client::ClientError::Api {error,..} if error.code=="application_busy")
    );
    api.finish_operation(
        &accepted.operation_id,
        Some(("image_resolution_rejected", "image unavailable")),
    )
    .await;
    let renamed = keyed
        .rename_application(&accepted.application_id, &request)
        .await
        .unwrap();
    assert_eq!(renamed.application_id, accepted.application_id);
    assert_eq!(renamed.generation, 2);
    let replay = keyed
        .rename_application(&accepted.application_id, &request)
        .await
        .unwrap();
    assert_eq!(replay.generation, 2);
    let app = api
        .client
        .application(&accepted.application_id)
        .await
        .unwrap();
    assert_eq!(app.application.metadata.name, "renamed");
    assert_eq!(
        api.client
            .operation(&accepted.operation_id)
            .await
            .unwrap()
            .state,
        piqueld_core::OperationState::Failed
    );
    let mut identical = AcceptanceApi::request();
    identical.manifest.metadata.name = "renamed".into();
    identical.expected_generation = Some(2);
    identical.expected_application_id = Some(accepted.application_id.clone());
    let no_op = api.client.apply_application(&identical).await.unwrap();
    assert_eq!(no_op.operation_id, accepted.operation_id);
    assert_eq!(no_op.generation, 2);
    assert_eq!(
        api.client
            .operation(&accepted.operation_id)
            .await
            .unwrap()
            .state,
        piqueld_core::OperationState::Failed
    );
    // The old name now identifies a different app. A captured ID must prevent overwriting it.
    let old_name = api
        .client
        .apply_application(&AcceptanceApi::request())
        .await
        .unwrap();
    let mut stale = AcceptanceApi::request();
    stale.expected_generation = Some(1);
    stale.expected_application_id = Some(accepted.application_id);
    stale.manifest.spec.services[0].replicas = 3;
    let error = api.client.apply_application(&stale).await.unwrap_err();
    assert!(
        matches!(error,piqueld_client::ClientError::Api {error,..} if error.code=="identity_conflict")
    );
    assert_eq!(
        api.client
            .application(&old_name.application_id)
            .await
            .unwrap()
            .application
            .spec
            .services[0]
            .replicas,
        1
    );
}

#[tokio::test]
async fn receipt_failure_rolls_back_acceptance_and_expired_keys_are_reusable() {
    use sqlx::Connection;
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let mut connection = sqlx::SqliteConnection::connect(&format!(
        "sqlite://{}",
        temp.path().join("state.db").display()
    ))
    .await
    .unwrap();
    sqlx::query("CREATE TRIGGER reject_receipt BEFORE INSERT ON request_receipts BEGIN SELECT RAISE(ABORT,'injected receipt failure'); END").execute(&mut connection).await.unwrap();
    let keyed = api.client.clone().with_request_id("atomic-command");
    let request = AcceptanceApi::request();
    assert!(keyed.apply_application(&request).await.is_err());
    assert!(api.store.list(None, 50).await.unwrap().items.is_empty());
    sqlx::query("DROP TRIGGER reject_receipt")
        .execute(&mut connection)
        .await
        .unwrap();
    let accepted = keyed.apply_application(&request).await.unwrap();
    sqlx::query("UPDATE request_receipts SET expires_at_ms=0")
        .execute(&mut connection)
        .await
        .unwrap();
    let refreshed = keyed
        .refresh_application(&accepted.application_id, Some(1))
        .await
        .unwrap();
    assert_ne!(refreshed.operation_id, accepted.operation_id);
}

#[tokio::test]
async fn preview_reuses_active_digests_and_redacts_manifest_and_runtime_configuration() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let mut request = AcceptanceApi::request();
    request.manifest.spec.services[0]
        .environment
        .insert("TOKEN".into(), "old-private-value".into());
    let normalized = request
        .manifest
        .clone()
        .validate()
        .unwrap()
        .normalize(piqueld_core::ApplicationId::parse("app-preview-01").unwrap());
    let runtime = FakeRuntime {
        instance: InstanceId::parse(api.store.instance_id()).unwrap(),
        unavailable: std::sync::atomic::AtomicBool::new(false),
    };
    let target = runtime
        .prepare(&normalized, &ResolutionSet::default())
        .await
        .unwrap();
    api.store
        .save_application(&normalized, Some(&target), Some(0))
        .await
        .unwrap();
    request.expected_generation = Some(1);
    request.expected_application_id = Some(normalized.id.to_string());
    request.manifest.spec.services[0].replicas = 3;
    request.manifest.spec.services[0]
        .environment
        .insert("TOKEN".into(), "new-private-value".into());
    request.manifest.spec.services[0].command = vec!["command-private-value".into()];
    let preview = api.client.plan_application(&request).await.unwrap();
    assert_eq!(preview.generation, 1);
    assert!(!preview.identical);
    assert!(
        preview
            .changes
            .iter()
            .any(|change| change.field == "services.web.replicas"
                && change.after.as_deref() == Some("3"))
    );
    assert!(
        !preview
            .plan
            .actions
            .iter()
            .any(|action| matches!(action.kind, piqueld_core::ActionKind::ResolveImage { .. }))
    );
    let json = serde_json::to_string(&preview).unwrap();
    for secret in [
        "old-private-value",
        "new-private-value",
        "command-private-value",
    ] {
        assert!(!json.contains(secret));
    }
    assert!(json.contains("redacted"));
    assert_eq!(
        api.store.get(&normalized.id).await.unwrap().generation,
        1,
        "preview is read-only"
    );
}

impl AcceptanceApi {
    fn assert_error(error: piqueld_client::ClientError, code: &str) {
        assert!(
            matches!(error, piqueld_client::ClientError::Api { error, .. } if error.code == code)
        );
    }

    async fn finish_operation(&self, id: &str, error: Option<(&str, &str)>) {
        use piqueld_core::OperationState;
        self.store
            .transition_operation(id, OperationState::Requested, OperationState::Running, None)
            .await
            .unwrap();
        let state = if error.is_some() {
            OperationState::Failed
        } else {
            OperationState::Succeeded
        };
        self.store
            .transition_operation(id, OperationState::Running, state, error)
            .await
            .unwrap();
    }

    async fn finish_deletion(&self, deletion: &AcceptedOperation) {
        self.store
            .transition_operation(
                &deletion.operation_id,
                piqueld_core::OperationState::Requested,
                piqueld_core::OperationState::Running,
                None,
            )
            .await
            .unwrap();
        let operation = self.store.operation(&deletion.operation_id).await.unwrap();
        self.store
            .finish_delete_operation(&operation)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn mutations_require_preconditions_but_refresh_and_reconcile_use_current_intent() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let mut request = AcceptanceApi::request();
    request.expected_generation = None;
    AcceptanceApi::assert_error(
        api.client.apply_application(&request).await.unwrap_err(),
        "precondition_required",
    );
    request.expected_generation = Some(0);
    let accepted = api.client.apply_application(&request).await.unwrap();
    request.expected_generation = Some(1);
    AcceptanceApi::assert_error(
        api.client.apply_application(&request).await.unwrap_err(),
        "precondition_required",
    );
    request.expected_application_id = Some(accepted.application_id.clone());
    request.manifest.spec.services[0].replicas = 2;
    let mut competing = request.clone();
    competing.manifest.spec.services[0].replicas = 3;
    let (first, second) = tokio::join!(
        api.client.apply_application(&request),
        api.client.apply_application(&competing)
    );
    assert_ne!(first.is_ok(), second.is_ok());
    AcceptanceApi::assert_error(first.err().or(second.err()).unwrap(), "generation_conflict");
    AcceptanceApi::assert_error(
        api.client
            .delete_application_with_generation(&accepted.application_id, None)
            .await
            .unwrap_err(),
        "precondition_required",
    );
    AcceptanceApi::assert_error(
        api.client
            .delete_application_with_generation(&accepted.application_id, Some(1))
            .await
            .unwrap_err(),
        "generation_conflict",
    );
    let refreshed = api
        .client
        .refresh_application(&accepted.application_id, None)
        .await
        .unwrap();
    assert_eq!(refreshed.generation, 2);
    let deletion = api
        .client
        .delete_application_with_preconditions(&accepted.application_id, Some(1), true)
        .await
        .unwrap();
    assert_eq!(deletion.generation, 3);
    let reconciled = api
        .client
        .reconcile_application(&accepted.application_id, None)
        .await
        .unwrap();
    assert_eq!(reconciled.operation_id, deletion.operation_id);
    assert!(
        api.client
            .refresh_application(&accepted.application_id, None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn forced_apply_retargets_reused_names_and_creates_absent_names() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let mut request = AcceptanceApi::request();
    request.expected_generation = None;
    let original = api
        .client
        .apply_application_with_force(&request, true)
        .await
        .unwrap();
    let deletion = api
        .client
        .delete_application_with_generation(&original.application_id, Some(1))
        .await
        .unwrap();
    api.finish_deletion(&deletion).await;
    let replacement = api
        .client
        .apply_application(&AcceptanceApi::request())
        .await
        .unwrap();
    assert_ne!(replacement.application_id, original.application_id);
    request.expected_generation = Some(1);
    request.expected_application_id = Some(original.application_id);
    request.manifest.spec.services[0].replicas = 4;
    AcceptanceApi::assert_error(
        api.client.apply_application(&request).await.unwrap_err(),
        "identity_conflict",
    );
    let forced = api
        .client
        .apply_application_with_force(&request, true)
        .await
        .unwrap();
    assert_eq!(forced.application_id, replacement.application_id);
    assert_eq!(forced.generation, 2);
    assert_eq!(
        api.client
            .application(&forced.application_id)
            .await
            .unwrap()
            .application
            .spec
            .services[0]
            .replicas,
        4
    );
    request.manifest.spec.services[0].replicas = 0;
    assert!(
        api.client
            .apply_application_with_force(&request, true)
            .await
            .is_err(),
        "force must not bypass manifest validation"
    );
    assert_eq!(
        api.client
            .application(&forced.application_id)
            .await
            .unwrap()
            .generation,
        2
    );
}

#[tokio::test]
async fn forced_receipts_replay_after_restart_without_overwriting_newer_intent() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let request = AcceptanceApi::request();
    let keyed = api.client.clone().with_request_id("forced-command");
    let accepted = keyed
        .apply_application_with_force(&request, true)
        .await
        .unwrap();
    let mut changed = request.clone();
    changed.manifest.spec.services[0].replicas = 3;
    let newer = api
        .client
        .apply_application_with_force(&changed, true)
        .await
        .unwrap();
    drop(api);
    let api = AcceptanceApi::start(&temp).await;
    let keyed = api.client.clone().with_request_id("forced-command");
    let replay = keyed
        .apply_application_with_force(&request, true)
        .await
        .unwrap();
    assert_eq!(replay.operation_id, accepted.operation_id);
    assert_eq!(replay.generation, accepted.generation);
    assert_eq!(
        api.client
            .operation(&accepted.operation_id)
            .await
            .unwrap()
            .state,
        piqueld_core::OperationState::Superseded
    );
    let current = api
        .client
        .application(&accepted.application_id)
        .await
        .unwrap();
    assert_eq!(current.generation, newer.generation);
    assert_eq!(current.application.spec.services[0].replicas, 3);
    AcceptanceApi::assert_error(
        keyed.apply_application(&request).await.unwrap_err(),
        "request_id_conflict",
    );
    let separately_invoked = api.client.clone().with_request_id("new-forced-command");
    let reapplied = separately_invoked
        .apply_application_with_force(&request, true)
        .await
        .unwrap();
    assert_eq!(reapplied.generation, newer.generation + 1);
}

#[tokio::test]
async fn forced_rename_bypasses_revision_but_preserves_busy_and_name_checks() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let accepted = api
        .client
        .apply_application(&AcceptanceApi::request())
        .await
        .unwrap();
    let mut rename = piqueld_client::RenameApplicationRequest {
        name: "renamed".into(),
        expected_generation: None,
    };
    AcceptanceApi::assert_error(
        api.client
            .rename_application(&accepted.application_id, &rename)
            .await
            .unwrap_err(),
        "precondition_required",
    );
    AcceptanceApi::assert_error(
        api.client
            .rename_application_with_force(&accepted.application_id, &rename, true)
            .await
            .unwrap_err(),
        "application_busy",
    );
    api.finish_operation(&accepted.operation_id, None).await;
    rename.expected_generation = Some(99);
    let renamed = api
        .client
        .rename_application_with_force(&accepted.application_id, &rename, true)
        .await
        .unwrap();
    assert_eq!(renamed.generation, 2);
    assert_eq!(renamed.application_id, accepted.application_id);
    api.client
        .apply_application(&AcceptanceApi::request())
        .await
        .unwrap();
    rename.name = "notes".into();
    AcceptanceApi::assert_error(
        api.client
            .rename_application_with_force(&accepted.application_id, &rename, true)
            .await
            .unwrap_err(),
        "application_name_collision",
    );
}

#[tokio::test]
async fn docker_outage_rejects_previews_and_new_apply_but_preserves_receipt_replay() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let request = AcceptanceApi::request();
    let keyed = api.client.clone().with_request_id("outage-replay");
    let accepted = keyed.apply_application(&request).await.unwrap();
    api.runtime
        .unavailable
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let replay = keyed.apply_application(&request).await.unwrap();
    assert_eq!(replay.operation_id, accepted.operation_id);
    let mut existing = request.clone();
    existing.expected_generation = Some(accepted.generation);
    existing.expected_application_id = Some(accepted.application_id.clone());
    let mut changed = existing.clone();
    changed.manifest.spec.services[0].replicas = 2;
    let mut fresh = request.clone();
    fresh.manifest.metadata.name = "new-application".into();
    for proposed in [&existing, &changed, &fresh] {
        for error in [
            api.client.plan_application(proposed).await.unwrap_err(),
            api.client.apply_application(proposed).await.unwrap_err(),
        ] {
            assert!(
                matches!(error, piqueld_client::ClientError::Api { status, error, .. }
                if status == axum::http::StatusCode::SERVICE_UNAVAILABLE && error.code == "docker_unavailable")
            );
        }
    }
    let mismatch = keyed.apply_application(&changed).await.unwrap_err();
    assert!(
        matches!(mismatch, piqueld_client::ClientError::Api { error, .. } if error.code == "request_id_conflict")
    );
    assert!(
        api.store
            .find_by_name("new-application")
            .await
            .unwrap()
            .is_none()
    );
    let stored = api.store.find_by_name("notes").await.unwrap().unwrap();
    assert_eq!(stored.generation, accepted.generation);
    assert_eq!(stored.application.spec.services[0].replicas, 1);
}
