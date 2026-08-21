//! Focused API/client coverage for the polling application lifecycle.

use async_trait::async_trait;
use axum::{body::Body, http::Request, serve};
use http_body_util::BodyExt;
use piqueld::api::{
    ApiState, BoundaryError, PreparedApplication, RuntimeBoundary, UiAssets, api_router, router,
    web_router,
};
use piqueld::store::{SqliteStore, StoredApplication};
use piqueld_client::{
    AcceptedOperation, Client, CreateApplicationRequest, DeleteApplicationRequest,
    PlanApplicationRequest, ReplaceApplicationRequest,
};
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
    reconcile_and_delete(&client, &created, replaced.generation).await;

    server.abort();
}

async fn assert_create_plan(client: &Client, manifest: &ApplicationManifest) {
    let preview = client
        .plan_create(&PlanApplicationRequest {
            manifest: manifest.clone(),
        })
        .await
        .expect("create preview succeeds");
    assert_eq!(preview.proposed_generation, 1);
    assert!(matches!(
        preview.plan.actions.as_slice(),
        [action] if matches!(action.kind, ActionKind::ResolveImage { .. })
    ));
}

async fn create_and_inspect(client: &Client, manifest: &ApplicationManifest) -> AcceptedOperation {
    let created = client
        .create_application(
            &CreateApplicationRequest {
                manifest: manifest.clone(),
            },
            "api-contract-create",
        )
        .await
        .expect("create succeeds");
    let replay = client
        .create_application(
            &CreateApplicationRequest {
                manifest: manifest.clone(),
            },
            "api-contract-create",
        )
        .await
        .expect("create retry succeeds");
    assert_eq!(created.operation_id, replay.operation_id);
    assert_eq!(client.applications().await.unwrap().items.len(), 1);
    assert_eq!(
        client
            .application(&created.application_id)
            .await
            .unwrap()
            .generation,
        1
    );
    assert_eq!(
        client
            .application_status(&created.application_id)
            .await
            .unwrap()
            .state,
        "pending"
    );
    let detail = client
        .application_detail(&created.application_id)
        .await
        .expect("application detail succeeds");
    assert_eq!(
        detail.application.application.id,
        created.application_id.parse().unwrap()
    );
    assert_eq!(detail.application.application.spec.services.len(), 1);
    assert_eq!(detail.observed.services[0].convergence, "failed");
    assert!(!detail.diagnostics.is_empty());
    assert_eq!(
        client.operation(&created.operation_id).await.unwrap().kind,
        "create"
    );
    created
}

#[tokio::test]
async fn dashboard_fallback_preserves_api_and_asset_route_precedence() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let ui_dir = temp.path().join("ui");
    std::fs::create_dir_all(&ui_dir).expect("UI directory is created");
    std::fs::write(
        ui_dir.join("index.html"),
        "<!doctype html><html><body><main>dashboard-shell</main></body></html>",
    )
    .expect("dashboard shell is written");
    std::fs::write(ui_dir.join("app.js"), "console.log('dashboard');")
        .expect("dashboard asset is written");

    let application = web_router(state(&temp).await, UiAssets::Directory(ui_dir));
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

    let missing = web_router(
        state(temp).await,
        UiAssets::Directory(temp.path().join("missing-ui")),
    );
    let unavailable = response_text(
        missing
            .oneshot(request("/dashboard/"))
            .await
            .expect("missing UI request succeeds"),
    )
    .await;
    assert_eq!(unavailable.0, axum::http::StatusCode::SERVICE_UNAVAILABLE);
}

fn request(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("request is valid")
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
    manifest: ApplicationManifest,
) -> AcceptedOperation {
    let replaced = client
        .replace_application(
            &created.application_id,
            &ReplaceApplicationRequest {
                expected_generation: 1,
                manifest: manifest.clone(),
            },
        )
        .await
        .expect("replacement succeeds");
    assert_eq!(replaced.generation, 2);
    let planned = client
        .plan_replace(
            &created.application_id,
            &piqueld_client::ReplacePlanRequest {
                expected_generation: 2,
                manifest,
            },
        )
        .await
        .expect("replacement preview succeeds");
    assert_eq!(planned.proposed_generation, 3);
    replaced
}

async fn reconcile_and_delete(client: &Client, created: &AcceptedOperation, generation: u64) {
    let reconciled = client
        .reconcile(&created.application_id, generation)
        .await
        .expect("reconcile succeeds");
    assert_eq!(
        client
            .operation(&reconciled.operation_id)
            .await
            .unwrap()
            .kind,
        "reconcile"
    );

    let deleted = client
        .delete_application(
            &created.application_id,
            &DeleteApplicationRequest {
                expected_generation: generation,
            },
        )
        .await
        .expect("delete succeeds");
    assert_eq!(deleted.generation, 3);
    assert_eq!(
        client.operation(&deleted.operation_id).await.unwrap().kind,
        "delete"
    );
}

// ---------------------------------------------------------------------------
// Raw-transport helpers and negative-path coverage restored from the former
// in-crate API suite.
// ---------------------------------------------------------------------------

use http::{HeaderMap, HeaderValue, Method, Request as HttpRequest, StatusCode, Uri};
use http_body_util::{BodyExt, Full};
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
        "/api/v1/applications",
        &[
            ("content-type", "application/json"),
            ("idempotency-key", "k"),
        ],
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
    // route; /api/v1/applications supports GET (list) and POST (create).
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
        Some(&HeaderValue::from_static("GET, POST"))
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
        Some(&HeaderValue::from_static("GET, PUT, DELETE"))
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
        "expected_generation": 1,
        "surprise": true
    });
    let unknown = send_raw(
        Target::Tcp(address),
        Method::DELETE,
        "/api/v1/applications/someappid01",
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
        "/api/v1/applications",
        &[
            ("content-type", "application/toml"),
            ("idempotency-key", "toml-create-key"),
        ],
        valid_toml.as_bytes().to_vec(),
    )
    .await;
    assert_eq!(created.status, StatusCode::ACCEPTED);
    assert!(created.body["data"]["operation_id"].is_string());

    server.abort();
}

#[tokio::test]
async fn concurrent_keyed_creates_have_exactly_one_durable_winner() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(serve(listener, router(state(&temp).await)).into_future());
    let client =
        std::sync::Arc::new(Client::tcp(&format!("http://{address}/")).expect("valid endpoint"));

    let manifest_a = manifest();
    let mut manifest_b = manifest();
    manifest_b.spec.services[0].replicas = 3;

    let attempts: Vec<_> = (0..8)
        .map(|_| {
            let client = std::sync::Arc::clone(&client);
            let manifest = manifest_a.clone();
            tokio::spawn(async move {
                client
                    .create_application(&CreateApplicationRequest { manifest }, "concurrent-key")
                    .await
            })
        })
        .collect();
    let mut operation_ids = Vec::new();
    for attempt in attempts {
        let accepted = attempt.await.expect("task joins").expect("create succeeds");
        operation_ids.push(accepted.operation_id);
    }
    operation_ids.sort();
    operation_ids.dedup();
    assert_eq!(operation_ids.len(), 1, "one durable winner");

    let conflict = client
        .create_application(
            &CreateApplicationRequest {
                manifest: manifest_b.clone(),
            },
            "concurrent-key",
        )
        .await
        .expect_err("different body with same key conflicts");
    match conflict {
        piqueld_client::ClientError::Api { status, error } => {
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(error.code, "idempotency_key_reused");
        }
        other => panic!("unexpected error: {other:?}"),
    }

    // Distinct fresh names racing concurrently both succeed.
    let mut manifest_other = manifest();
    manifest_other.metadata.name = "fourth".into();
    let mut manifest_left = manifest();
    manifest_left.metadata.name = "third".into();
    let left = {
        let client = std::sync::Arc::clone(&client);
        let manifest = manifest_left;
        tokio::spawn(async move {
            client
                .create_application(&CreateApplicationRequest { manifest }, "race-left")
                .await
        })
    };
    let right = {
        let client = std::sync::Arc::clone(&client);
        tokio::spawn(async move {
            client
                .create_application(
                    &CreateApplicationRequest {
                        manifest: manifest_other,
                    },
                    "race-right",
                )
                .await
        })
    };
    let _ = manifest_b;
    let left = left.await.expect("joins").expect("left wins");
    let right = right.await.expect("joins").expect("right wins");
    assert_ne!(left.application_id, right.application_id);

    server.abort();
}

#[tokio::test]
async fn create_replay_returns_the_original_result_after_a_later_replacement() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(serve(listener, router(state(&temp).await)).into_future());
    let client = Client::tcp(&format!("http://{address}/")).expect("valid endpoint");
    let manifest = manifest();

    let created = create_and_inspect(&client, &manifest).await;
    let replaced = replace_and_plan(&client, &created, manifest.clone()).await;
    assert_eq!(replaced.generation, 2);

    let replay = client
        .create_application(
            &CreateApplicationRequest { manifest },
            "api-contract-create",
        )
        .await
        .expect("replay replays the original create");
    assert_eq!(replay.operation_id, created.operation_id);
    assert_eq!(
        client
            .application(&created.application_id)
            .await
            .expect("application remains")
            .generation,
        2,
        "the replacement still owns the application"
    );

    server.abort();
}

#[tokio::test]
async fn active_reconcile_dedupe_does_not_repeat_runtime_io() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let store = Arc::new(
        SqliteStore::open(temp.path().join("state.db"))
            .await
            .expect("fresh database opens"),
    );
    let instance = InstanceId::parse(store.instance_id().to_owned()).expect("instance id");
    let runtime = Arc::new(CountingRuntime {
        instance,
        prepares: std::sync::atomic::AtomicUsize::new(0),
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(
        serve(
            listener,
            router(ApiState::new(Arc::clone(&store), runtime.clone())),
        )
        .into_future(),
    );
    let client = Client::tcp(&format!("http://{address}/")).expect("valid endpoint");

    let created = client
        .create_application(
            &CreateApplicationRequest {
                manifest: manifest(),
            },
            "dedupe-create",
        )
        .await
        .expect("create succeeds");

    let first = client
        .reconcile(&created.application_id, 1)
        .await
        .expect("first reconcile succeeds");
    let second = client
        .reconcile(&created.application_id, 1)
        .await
        .expect("second reconcile dedupes");
    assert_eq!(first.operation_id, second.operation_id);
    assert_eq!(
        runtime.prepares.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "only create resolves inputs; the deduped reconcile enqueues without runtime IO"
    );

    server.abort();
}

struct CountingRuntime {
    instance: InstanceId,
    prepares: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl RuntimeBoundary for CountingRuntime {
    async fn prepare(
        &self,
        application: &NormalizedApplication,
    ) -> Result<PreparedApplication, BoundaryError> {
        self.prepares
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        FakeRuntime {
            instance: self.instance.clone(),
        }
        .prepare(application)
        .await
    }

    async fn observe(
        &self,
        application: &StoredApplication,
    ) -> Result<ObservedApplication, BoundaryError> {
        FakeRuntime {
            instance: self.instance.clone(),
        }
        .observe(application)
        .await
    }
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

    let loopback = send_raw(
        Target::Tcp(address),
        Method::GET,
        "/api/v1/system/status",
        &[("host", &format!("127.0.0.1:{port}", port = address.port()))],
        Vec::new(),
    )
    .await;
    assert_eq!(loopback.status, StatusCode::OK);

    let ipv6 = send_raw(
        Target::Tcp(address),
        Method::GET,
        "/api/v1/system/status",
        &[("host", "[::1]:9999")],
        Vec::new(),
    )
    .await;
    assert_eq!(ipv6.status, StatusCode::OK);

    server.abort();
}

#[tokio::test]
async fn duplicate_idempotency_keys_are_rejected() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(serve(listener, router(state(&temp).await)).into_future());

    let response = send_raw(
        Target::Tcp(address),
        Method::POST,
        "/api/v1/applications",
        &[
            ("content-type", "application/json"),
            ("idempotency-key", "one"),
            ("idempotency-key", "two"),
        ],
        b"{}".to_vec(),
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(response.code(), "idempotency_key_invalid");

    server.abort();
}

#[tokio::test]
async fn expected_generation_header_is_parsed_strictly() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(serve(listener, router(state(&temp).await)).into_future());

    for value in ["+3", "-3", " ", "1.5", "18446744073709551615"] {
        let response = send_raw(
            Target::Tcp(address),
            Method::PUT,
            "/api/v1/applications/app123456789",
            &[
                ("content-type", "application/toml"),
                ("x-expected-generation", value),
            ],
            b"api_version = 'piqueld.dev/v1alpha1'".to_vec(),
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "value {value}");
    }

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
    let temp = tempfile::tempdir().expect("temporary directory");
    let data_dir = temp.path().join("state");
    std::fs::create_dir(&data_dir).expect("data dir exists");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700))
            .expect("data dir is private");
    }
    piqueld::prepare_data_dir(&data_dir)
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
    reconcile_and_delete(&client, &created, replaced.generation).await;

    server.abort();
}
