//! Focused API/client coverage for the polling application lifecycle.

use async_trait::async_trait;
use axum::{body::Body, http::Request, serve};
use http_body_util::BodyExt;
use piqueld::api::{ApiState, EmbeddedBundle, UiAssets, api_router, router, web_router};
use piqueld::application::{BoundaryError, PreparedApplication, RuntimeBoundary};
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
}

#[async_trait]
impl RuntimeBoundary for FakeRuntime {
    async fn prepare(
        &self,
        application: &NormalizedApplication,
    ) -> Result<PreparedApplication, BoundaryError> {
        let sources = application
            .spec
            .services
            .iter()
            .map(|service| {
                let Source::Image { image } = &service.source;
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
        Ok(PreparedApplication {
            resolved,
            observed: ObservedApplication::default(),
        })
    }

    async fn observe(
        &self,
        _application: &StoredApplication,
    ) -> Result<ObservedApplication, BoundaryError> {
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
    ApiState::new(Arc::clone(&store), Arc::new(FakeRuntime { instance }))
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
    delete(&client, &created).await;

    server.abort();
}

async fn assert_create_plan(client: &Client, manifest: &ApplicationManifest) {
    let preview = client
        .plan_application(&ApplyApplicationRequest {
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
    let request = ApplyApplicationRequest {
        manifest: manifest.clone(),
    };
    let created = client
        .apply_application(&request)
        .await
        .expect("apply succeeds");
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
    assert_eq!(
        detail.observed.services[0].convergence,
        piqueld_core::Convergence::Updating
    );
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
    let request = ApplyApplicationRequest { manifest };
    let replaced = client
        .apply_application(&request)
        .await
        .expect("replacement succeeds");
    assert_eq!(replaced.application_id, created.application_id);
    client
        .plan_application(&request)
        .await
        .expect("preview succeeds");
    replaced
}

async fn delete(client: &Client, created: &AcceptedOperation) {
    let deleted = client
        .delete_application(&created.application_id)
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
        &[("content-type", "application/toml")],
        valid_toml.as_bytes().to_vec(),
    )
    .await;
    assert_eq!(created.status, StatusCode::ACCEPTED);
    assert!(created.body["data"]["operation_id"].is_string());

    server.abort();
}

#[tokio::test]
async fn host_allowlist_blocks_foreign_authorities() {
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
    assert_eq!(rebinding.status, StatusCode::FORBIDDEN);
    assert_eq!(rebinding.code(), "host_not_allowed");
    // Allowlist rejections are structured errors too: their body must carry
    // the same request ID the response headers advertise.
    let header_id = rebinding
        .headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok());
    if let Some(header_id) = header_id {
        assert_eq!(rebinding.request_id(), Some(header_id));
    } else {
        panic!("rejected host response carries no x-request-id header");
    }

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
    assert_eq!(malformed_ipv6.status, StatusCode::FORBIDDEN);

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

    // A non-loopback IPv6 literal stays rejected.
    let foreign_ipv6 = send_raw(
        Target::Tcp(address),
        Method::GET,
        "/api/v1/system/status",
        &[("host", "fe80::1")],
        Vec::new(),
    )
    .await;
    assert_eq!(foreign_ipv6.status, StatusCode::FORBIDDEN);
    assert_eq!(foreign_ipv6.code(), "host_not_allowed");

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
    delete(&client, &created).await;

    server.abort();
}
