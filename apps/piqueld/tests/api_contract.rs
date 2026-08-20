//! Focused API/client coverage for the polling application lifecycle.

use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode, header},
    serve,
};
use http_body_util::BodyExt;
use piqueld::api::{
    ApiState, BoundaryError, PreparedApplication, RuntimeBoundary, api_router, router,
};
use piqueld::store::{SqliteStore, StoredApplication};
use piqueld::transfer::ARCHIVE_CONTENT_TYPE;
use piqueld_client::{
    AcceptedOperation, Client, CreateApplicationRequest, DeleteApplicationRequest, Envelope,
    OperationView, PlanApplicationRequest, ReplaceApplicationRequest, StateImportConfirmation,
    StateImportResult,
};
use piqueld_core::{
    InstanceId, NormalizedApplication, ObservedApplication, ResolutionSet, compile_application,
    manifest::{ApplicationManifest, Source},
    planner::ActionKind,
    resource::ResolvedSource,
};
use sha2::{Digest, Sha256};
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
                let Source::Image { image } = &service.source else {
                    unreachable!("API contract fixture uses image sources")
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
            &ResolutionSet {
                sources,
                secrets: std::collections::BTreeMap::default(),
            },
        )
        .map_err(BoundaryError::Compilation)?;
        Ok(PreparedApplication {
            resolved,
            observed: ObservedApplication::default(),
            builds: Vec::new(),
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
    ApiState::new(&store, Arc::new(FakeRuntime { instance }))
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
    let mut events = client.watch_operation(&created.operation_id, None);
    let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
        .await
        .expect("operation event arrives")
        .expect("operation event stream remains open")
        .expect("operation event is valid");
    assert_eq!(event.event.as_deref(), Some("operation"));
    let streamed: OperationView = serde_json::from_str(&event.data).expect("operation JSON");
    assert_eq!(streamed.id, created.operation_id);
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

    let application = router(state(&temp).await.with_ui_dir(ui_dir));

    let root = response_text(
        application
            .clone()
            .oneshot(request("/"))
            .await
            .expect("root request succeeds"),
    )
    .await;
    assert_eq!(root.0, axum::http::StatusCode::OK);
    assert!(root.1.contains("dashboard-shell"));

    let deep_link = response_text(
        application
            .clone()
            .oneshot(request("/applications/notes"))
            .await
            .expect("deep link request succeeds"),
    )
    .await;
    assert_eq!(deep_link.0, axum::http::StatusCode::OK);
    assert!(deep_link.1.contains("dashboard-shell"));

    let asset = response_text(
        application
            .clone()
            .oneshot(request("/app.js"))
            .await
            .expect("asset request succeeds"),
    )
    .await;
    assert_eq!(asset.0, axum::http::StatusCode::OK);
    assert!(asset.1.contains("console.log"));

    let missing_asset = response_text(
        application
            .clone()
            .oneshot(request("/missing.js"))
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

    let api_only = api_router(state(&temp).await);
    let unix_root = response_text(
        api_only
            .oneshot(request("/"))
            .await
            .expect("API-only root request succeeds"),
    )
    .await;
    assert_eq!(unix_root.0, axum::http::StatusCode::NOT_FOUND);
    assert!(unix_root.1.contains("endpoint_not_found"));
}

#[tokio::test]
async fn state_transfer_is_binary_and_confirmation_is_single_use() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let application = api_router(state(&temp).await);

    let application_id = create_transfer_application(&application).await;

    let exported_application = application
        .clone()
        .oneshot(request(&format!(
            "/api/v1/applications/{application_id}/export"
        )))
        .await
        .expect("application export request succeeds");
    assert_eq!(exported_application.status(), StatusCode::OK);
    assert_eq!(
        exported_application
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/toml")
    );

    let exported = application
        .clone()
        .oneshot(request("/api/v1/state/export"))
        .await
        .expect("state export request succeeds");
    assert_eq!(exported.status(), StatusCode::OK);
    assert_eq!(
        exported
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some(ARCHIVE_CONTENT_TYPE)
    );
    let archive = exported
        .into_body()
        .collect()
        .await
        .expect("archive body is readable")
        .to_bytes();
    assert!(!archive.is_empty());
    let archive_digest = format!("sha256:{:x}", Sha256::digest(&archive));

    let confirmation = application
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/v1/state/import/confirm",
            "application/json",
            "",
            "",
            serde_json::to_vec(&serde_json::json!({"archive_digest": archive_digest}))
                .expect("confirmation request serializes"),
        ))
        .await
        .expect("confirmation request succeeds");
    assert_eq!(confirmation.status(), StatusCode::OK);
    let confirmation_body = confirmation
        .into_body()
        .collect()
        .await
        .expect("confirmation body is readable")
        .to_bytes();
    let confirmation: Envelope<StateImportConfirmation> =
        serde_json::from_slice(&confirmation_body).expect("confirmation response is typed");

    let imported = application
        .clone()
        .oneshot(archive_request(archive.to_vec(), &confirmation.data.token))
        .await
        .expect("state import request succeeds");
    assert_eq!(imported.status(), StatusCode::OK);
    let imported_body = imported
        .into_body()
        .collect()
        .await
        .expect("import body is readable")
        .to_bytes();
    let imported: Envelope<StateImportResult> =
        serde_json::from_slice(&imported_body).expect("import response is typed");
    assert_eq!(imported.data.applications_imported, 1);

    let replay = application
        .oneshot(archive_request(archive.to_vec(), &confirmation.data.token))
        .await
        .expect("replayed import request is handled");
    assert_eq!(replay.status(), StatusCode::PRECONDITION_FAILED);
}

async fn create_transfer_application(application: &Router) -> String {
    let response = application
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/v1/applications",
            "application/json",
            "idempotency-key",
            "transfer-test",
            serde_json::to_vec(&CreateApplicationRequest {
                manifest: manifest(),
            })
            .expect("create request serializes"),
        ))
        .await
        .expect("application create request succeeds");
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("create body is readable")
        .to_bytes();
    let created: Envelope<AcceptedOperation> =
        serde_json::from_slice(&body).expect("create response is typed");
    created.data.application_id
}

fn request(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("request is valid")
}

fn json_request(
    method: Method,
    uri: &str,
    content_type: &str,
    header_name: &str,
    header_value: &str,
    body: Vec<u8>,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, content_type);
    if !header_name.is_empty() {
        builder = builder.header(header_name, header_value);
    }
    builder.body(Body::from(body)).expect("request is valid")
}

fn archive_request(body: Vec<u8>, confirmation: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri("/api/v1/state/import")
        .header(header::CONTENT_TYPE, ARCHIVE_CONTENT_TYPE)
        .header("x-replace-confirmation", confirmation)
        .body(Body::from(body))
        .expect("archive request is valid")
}

async fn response_text(response: axum::response::Response) -> (axum::http::StatusCode, String) {
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("response body is readable")
        .to_bytes();
    (
        status,
        String::from_utf8(body.into_iter().collect()).expect("response is UTF-8"),
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
