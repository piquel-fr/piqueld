//! Versioned HTTP/JSON API and streaming event boundary.
#![allow(missing_docs)]

use async_trait::async_trait;
use axum::{
    Extension, Router,
    body::Body,
    extract::Request,
    http::{HeaderMap, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response, sse::Event},
};
use futures_util::Stream;
use piqueld_client::{
    AcceptedOperation, CreateApplicationRequest, Envelope, ErrorBody, PlanApplicationRequest,
    ReplaceApplicationRequest,
};
use piqueld_core::{
    ApplicationId, ApplicationIdError, NormalizedApplication, ObservedApplication,
    resource::ResolvedApplication,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{convert::Infallible, pin::Pin, sync::Arc};
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, RequestId, SetRequestIdLayer},
    trace::TraceLayer,
};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::store::{SqliteStore, StoreError, StoredApplication};

mod applications;
mod openapi;
mod operations;
mod system;

pub use openapi::openapi_document;

const JSON: &str = "application/json";
const TOML: &str = "application/toml";
const MAX_PAGE: usize = 100;

#[derive(Clone, Debug)]
pub struct RuntimeCapabilities {
    pub source_resolution: bool,
    pub runtime_observation: bool,
    pub runtime_execution: bool,
    pub reason: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PreparedApplication {
    pub resolved: ResolvedApplication,
    pub observed: ObservedApplication,
}

#[derive(Debug, thiserror::Error)]
pub enum BoundaryError {
    #[error("runtime capability is unavailable")]
    Unavailable,
    #[error("runtime request failed")]
    Failed,
}

/// Source resolution, runtime observation, and execution seam supplied by Plan 06.
#[async_trait]
pub trait RuntimeBoundary: Send + Sync + 'static {
    fn capabilities(&self) -> RuntimeCapabilities;
    async fn prepare(
        &self,
        application: &NormalizedApplication,
    ) -> Result<PreparedApplication, BoundaryError>;
    async fn observe(
        &self,
        application: &StoredApplication,
    ) -> Result<ObservedApplication, BoundaryError>;
}

/// Honest production boundary until Docker reconciliation lands in Plan 06.
pub struct UnavailableRuntime;

#[async_trait]
impl RuntimeBoundary for UnavailableRuntime {
    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            source_resolution: false,
            runtime_observation: false,
            runtime_execution: false,
            reason: Some("Docker source resolution and execution arrive in Plan 06".into()),
        }
    }
    async fn prepare(
        &self,
        _: &NormalizedApplication,
    ) -> Result<PreparedApplication, BoundaryError> {
        Err(BoundaryError::Unavailable)
    }
    async fn observe(&self, _: &StoredApplication) -> Result<ObservedApplication, BoundaryError> {
        Err(BoundaryError::Unavailable)
    }
}

#[derive(Clone)]
pub struct ApiState {
    store: Arc<SqliteStore>,
    runtime: Arc<dyn RuntimeBoundary>,
    instance_id: String,
    create_lock: Arc<tokio::sync::Mutex<()>>,
}

impl ApiState {
    #[must_use]
    pub fn new(store: Arc<SqliteStore>, runtime: Arc<dyn RuntimeBoundary>) -> Self {
        Self {
            instance_id: store.instance_id().to_owned(),
            store,
            runtime,
            create_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    details: Value,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
            details: Value::Null,
        }
    }
    fn details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }
}

impl From<StoreError> for ApiError {
    fn from(value: StoreError) -> Self {
        match value {
            StoreError::NotFound => {
                Self::new(StatusCode::NOT_FOUND, "not_found", "resource was not found")
            }
            StoreError::AlreadyExists => Self::new(
                StatusCode::CONFLICT,
                "application_name_collision",
                "application identity or name already exists",
            ),
            StoreError::IdempotencyConflict => Self::new(
                StatusCode::CONFLICT,
                "idempotency_key_reused",
                "Idempotency-Key was already used for a different request",
            ),
            StoreError::GenerationConflict { expected, actual } => Self::new(
                StatusCode::CONFLICT,
                "application_generation_conflict",
                "the application was modified by another client",
            )
            .details(json!({"expected_generation": expected, "current_generation": actual})),
            StoreError::MissingSecrets(names) => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "logical_secret_missing",
                "one or more referenced logical secrets do not exist",
            )
            .details(json!({"names": names})),
            StoreError::IllegalTransition => Self::new(
                StatusCode::CONFLICT,
                "application_state_conflict",
                "the requested application transition is not allowed",
            ),
            StoreError::InvalidInput => Self::new(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "the request is invalid",
            ),
            StoreError::SchemaMismatch => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "schema_mismatch",
                "database schema is incompatible",
            ),
            StoreError::Corrupt => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "stored_state_corrupt",
                "stored application state is corrupt",
            ),
            StoreError::Database => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "storage_unavailable",
                "control-plane storage is unavailable",
            ),
        }
    }
}

impl From<BoundaryError> for ApiError {
    fn from(value: BoundaryError) -> Self {
        match value {
            BoundaryError::Unavailable => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "runtime_unavailable",
                "runtime source resolution or execution is unavailable",
            ),
            BoundaryError::Failed => Self::new(
                StatusCode::BAD_GATEWAY,
                "runtime_request_failed",
                "runtime request failed",
            ),
        }
    }
}

impl From<ApplicationIdError> for ApiError {
    fn from(_: ApplicationIdError) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "application_id_invalid",
            "application ID is invalid",
        )
    }
}

impl From<piqueld_core::ValidationErrors> for ApiError {
    fn from(errors: piqueld_core::ValidationErrors) -> Self {
        if errors
            .0
            .iter()
            .all(|error| error.code == "manifest_decode_failed")
        {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "toml_malformed",
                "request TOML is malformed or does not match the application schema",
            )
        } else {
            let piqueld_core::ValidationErrors(errors) = errors;
            Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "manifest_validation_failed",
                "application manifest failed validation",
            )
            .details(json!({"errors": errors}))
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            code: self.code.into(),
            message: self.message.into(),
            details: self.details,
            request_id: uuid::Uuid::now_v7().simple().to_string(),
        };
        (
            self.status,
            [(header::CONTENT_TYPE, JSON)],
            axum::Json(body),
        )
            .into_response()
    }
}

/// Builds the complete Plan 05 router. No later-plan endpoints are published.
pub fn router(state: ApiState) -> Router {
    let request_id = header::HeaderName::from_static("x-request-id");
    let (router, openapi) = documented_router().split_for_parts();
    router
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(fallback)
        .with_state(state)
        .layer(Extension(Arc::new(openapi)))
        .layer(PropagateRequestIdLayer::new(request_id.clone()))
        .layer(middleware::from_fn(bind_error_request_id))
        .layer(SetRequestIdLayer::new(request_id, MakeRequestUuid))
        .layer(TraceLayer::new_for_http())
}

// Public endpoints must be registered through `routes!` here so Axum and the
// generated OpenAPI document receive the same method and path at the same time.
fn documented_router() -> OpenApiRouter<ApiState> {
    OpenApiRouter::with_openapi(openapi::base_document())
        .routes(routes!(system::status))
        .routes(routes!(system::capabilities))
        .routes(routes!(openapi::openapi))
        .routes(routes!(applications::list, applications::create))
        .routes(routes!(applications::plan_create))
        .routes(routes!(
            applications::get,
            applications::replace,
            applications::delete
        ))
        .routes(routes!(applications::plan_replace))
        .routes(routes!(applications::reconcile))
        .routes(routes!(applications::status))
        .routes(routes!(applications::events))
        .routes(routes!(operations::get))
        .routes(routes!(operations::events))
}

async fn bind_error_request_id(request: Request, next: Next) -> Response {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .and_then(|value| value.header_value().to_str().ok())
        .map(str::to_owned);
    let response = next.run(request).await;
    if !response.status().is_client_error() && !response.status().is_server_error() {
        return response;
    }
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with(JSON));
    if !is_json {
        return response;
    }
    let (parts, body) = response.into_parts();
    let Ok(bytes) = http_body_util::BodyExt::collect(body)
        .await
        .map(http_body_util::Collected::to_bytes)
    else {
        return Response::from_parts(parts, Body::empty());
    };
    let Ok(mut error) = serde_json::from_slice::<ErrorBody>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    if let Some(request_id) = request_id {
        error.request_id = request_id;
    }
    let bytes = serde_json::to_vec(&error).unwrap_or_else(|_| b"{}".to_vec());
    Response::from_parts(parts, Body::from(bytes))
}

type EventStream = Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>;

async fn fallback(method: Method) -> ApiError {
    let _ = method;
    ApiError::new(
        StatusCode::NOT_FOUND,
        "endpoint_not_found",
        "API endpoint was not found",
    )
}
async fn method_not_allowed() -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "HTTP method is not allowed",
    )
}

fn ok<T: Serialize>(data: T) -> impl IntoResponse {
    (StatusCode::OK, axum::Json(Envelope { data }))
}
fn accepted(data: AcceptedOperation) -> Response {
    (StatusCode::ACCEPTED, axum::Json(Envelope { data })).into_response()
}
fn generation(expected: u64, actual: u64) -> Result<(), ApiError> {
    if expected == actual {
        Ok(())
    } else {
        Err(ApiError::from(StoreError::GenerationConflict {
            expected,
            actual,
        }))
    }
}
fn valid_expected_generation(expected: u64) -> Result<(), ApiError> {
    if expected == 0 {
        Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "expected_generation_invalid",
            "expected generation must be a positive integer",
        ))
    } else {
        Ok(())
    }
}
fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}
fn require_json(headers: &HeaderMap) -> Result<(), ApiError> {
    match content_type(headers) {
        Some(value) if value.eq_ignore_ascii_case(JSON) => Ok(()),
        _ => Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content_type_unsupported",
            "Content-Type must be application/json",
        )),
    }
}
fn content_type(headers: &HeaderMap) -> Option<&str> {
    header_text(headers, "content-type").map(|v| v.split(';').next().unwrap_or(v).trim())
}
fn decode_json<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "json_malformed",
            "request JSON is malformed or contains unknown fields",
        )
    })
}

#[derive(Clone, Copy)]
enum RequestShape {
    Create,
    PlanCreate,
}
fn parse_manifest(
    headers: &HeaderMap,
    body: &[u8],
    shape: RequestShape,
) -> Result<piqueld_core::ValidatedApplication, ApiError> {
    match content_type(headers) {
        Some(value) if value.eq_ignore_ascii_case(JSON) => {
            let manifest = match shape {
                RequestShape::Create => decode_json::<CreateApplicationRequest>(body)?.manifest,
                RequestShape::PlanCreate => decode_json::<PlanApplicationRequest>(body)?.manifest,
            };
            let encoded = serde_json::to_string(&manifest).map_err(|_| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "manifest_invalid",
                    "application manifest is invalid",
                )
            })?;
            Ok(piqueld_core::parse_json(&encoded)?)
        }
        Some(value)
            if value.eq_ignore_ascii_case(TOML) || value.eq_ignore_ascii_case("text/toml") =>
        {
            std::str::from_utf8(body)
                .map_err(|_| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "toml_malformed",
                        "request TOML is malformed",
                    )
                })
                .and_then(|v| Ok(piqueld_core::parse_toml(v)?))
        }
        _ => Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content_type_unsupported",
            "Content-Type must be application/json or application/toml",
        )),
    }
}

fn parse_update(
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(piqueld_core::ValidatedApplication, u64), ApiError> {
    match content_type(headers) {
        Some(value) if value.eq_ignore_ascii_case(JSON) => {
            let request: ReplaceApplicationRequest = decode_json(body)?;
            valid_expected_generation(request.expected_generation)?;
            let encoded = serde_json::to_string(&request.manifest).map_err(|_| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "manifest_invalid",
                    "application manifest is invalid",
                )
            })?;
            Ok((
                piqueld_core::parse_json(&encoded)?,
                request.expected_generation,
            ))
        }
        Some(value)
            if value.eq_ignore_ascii_case(TOML) || value.eq_ignore_ascii_case("text/toml") =>
        {
            let expected = header_text(headers, "x-expected-generation")
                .ok_or_else(|| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "expected_generation_required",
                        "X-Expected-Generation is required for TOML replacement",
                    )
                })?
                .parse()
                .map_err(|_| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "expected_generation_invalid",
                        "expected generation is invalid",
                    )
                })?;
            valid_expected_generation(expected)?;
            let text = std::str::from_utf8(body).map_err(|_| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "toml_malformed",
                    "request TOML is malformed",
                )
            })?;
            Ok((piqueld_core::parse_toml(text)?, expected))
        }
        _ => Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content_type_unsupported",
            "Content-Type must be application/json or application/toml",
        )),
    }
}

fn idempotent_application_id(key: &str) -> ApplicationId {
    let digest = Sha256::digest(format!("piqueld-create/v1\0{key}").as_bytes());
    ApplicationId::parse(format!("app-{}", hex(&digest[..16])))
        .expect("digest application ID is valid")
}
fn idempotency_key_hash(key: &str) -> String {
    format!("sha256:{}", hex(&Sha256::digest(key.as_bytes())))
}
fn current_state_event_id(kind: &str, data: &str) -> String {
    let digest = Sha256::digest(format!("piqueld-sse/v1\0{kind}\0{data}").as_bytes());
    format!("current:{}", hex(&digest[..16]))
}
fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        value.push(char::from(HEX[usize::from(byte >> 4)]));
        value.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    value
}
fn last_event_id(headers: &HeaderMap) -> Option<String> {
    header_text(headers, "last-event-id").map(str::to_owned)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::needless_pass_by_value)]
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use piqueld_core::resource::ResolvedSource;
    use piqueld_core::{InstanceId, ResolutionSet, compile_application};
    use std::collections::BTreeMap;
    use tempfile::TempDir;
    use tower::ServiceExt;

    use crate::store::{ApplicationRepository, OperationRepository, StepState, WorkState};

    struct FakeRuntime {
        instance: InstanceId,
    }
    #[async_trait]
    impl RuntimeBoundary for FakeRuntime {
        fn capabilities(&self) -> RuntimeCapabilities {
            RuntimeCapabilities {
                source_resolution: true,
                runtime_observation: true,
                runtime_execution: true,
                reason: None,
            }
        }
        async fn prepare(
            &self,
            app: &NormalizedApplication,
        ) -> Result<PreparedApplication, BoundaryError> {
            let sources = app
                .spec
                .services
                .iter()
                .map(|service| {
                    let requested = match &service.source {
                        piqueld_core::manifest::Source::Image { image } => image.clone(),
                        piqueld_core::manifest::Source::Git { .. } => {
                            return Err(BoundaryError::Failed);
                        }
                    };
                    let repository = requested
                        .rsplit_once(':')
                        .map_or(requested.as_str(), |(name, _)| name);
                    Ok((
                        service.name.clone(),
                        ResolvedSource::Image {
                            digest_reference: format!("{repository}@sha256:{}", "a".repeat(64)),
                            requested,
                        },
                    ))
                })
                .collect::<Result<BTreeMap<_, _>, _>>()?;
            let resolved = compile_application(
                app,
                self.instance.clone(),
                "piqueld-ingress",
                &ResolutionSet {
                    sources,
                    secrets: BTreeMap::new(),
                },
            )
            .unwrap();
            Ok(PreparedApplication {
                resolved,
                observed: ObservedApplication::default(),
            })
        }
        async fn observe(
            &self,
            _: &StoredApplication,
        ) -> Result<ObservedApplication, BoundaryError> {
            Ok(ObservedApplication::default())
        }
    }

    struct PreviewOnlyRuntime;
    #[async_trait]
    impl RuntimeBoundary for PreviewOnlyRuntime {
        fn capabilities(&self) -> RuntimeCapabilities {
            RuntimeCapabilities {
                source_resolution: true,
                runtime_observation: false,
                runtime_execution: false,
                reason: None,
            }
        }
        async fn prepare(
            &self,
            _: &NormalizedApplication,
        ) -> Result<PreparedApplication, BoundaryError> {
            panic!("pure preview must not invoke source resolution")
        }
        async fn observe(
            &self,
            _: &StoredApplication,
        ) -> Result<ObservedApplication, BoundaryError> {
            panic!("create preview has no current runtime state to observe")
        }
    }

    struct ActiveRetryRuntime;
    #[async_trait]
    impl RuntimeBoundary for ActiveRetryRuntime {
        fn capabilities(&self) -> RuntimeCapabilities {
            RuntimeCapabilities {
                source_resolution: true,
                runtime_observation: true,
                runtime_execution: true,
                reason: None,
            }
        }
        async fn prepare(
            &self,
            _: &NormalizedApplication,
        ) -> Result<PreparedApplication, BoundaryError> {
            panic!("an active reconcile retry must not prepare an application")
        }
        async fn observe(
            &self,
            _: &StoredApplication,
        ) -> Result<ObservedApplication, BoundaryError> {
            panic!("an active reconcile retry must not repeat runtime observation")
        }
    }

    async fn fixture(runtime: Arc<dyn RuntimeBoundary>) -> (TempDir, Arc<SqliteStore>, Router) {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(
            SqliteStore::open(temp.path().join("state.db"))
                .await
                .unwrap(),
        );
        let app = router(ApiState::new(Arc::clone(&store), runtime));
        (temp, store, app)
    }
    async fn fake_fixture() -> (TempDir, Arc<SqliteStore>, Router) {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(
            SqliteStore::open(temp.path().join("state.db"))
                .await
                .unwrap(),
        );
        let instance = InstanceId::parse(store.instance_id().to_owned()).unwrap();
        let app = router(ApiState::new(
            Arc::clone(&store),
            Arc::new(FakeRuntime { instance }),
        ));
        (temp, store, app)
    }
    fn manifest() -> Value {
        json!({"api_version":"piqueld.dev/v1alpha1","kind":"Application","metadata":{"name":"notes"},"spec":{"services":[{"name":"web","source":{"type":"image","image":"ghcr.io/example/notes:1"}}]}})
    }
    fn request(method: Method, uri: &str, value: Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, JSON)
            .body(Body::from(value.to_string()))
            .unwrap()
    }
    async fn json_body(response: Response) -> Value {
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
    }
    fn assert_refs_resolve(value: &Value, schemas: &serde_json::Map<String, Value>) {
        match value {
            Value::Object(object) => {
                if let Some(Value::String(reference)) = object.get("$ref") {
                    let name = reference
                        .strip_prefix("#/components/schemas/")
                        .expect("only local component schema references are generated");
                    assert!(schemas.contains_key(name), "missing schema {name}");
                }
                for nested in object.values() {
                    assert_refs_resolve(nested, schemas);
                }
            }
            Value::Array(values) => {
                for nested in values {
                    assert_refs_resolve(nested, schemas);
                }
            }
            _ => {}
        }
    }

    #[tokio::test]
    async fn create_is_accepted_idempotent_and_queryable() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(
            SqliteStore::open(temp.path().join("state.db"))
                .await
                .unwrap(),
        );
        let instance = InstanceId::parse(store.instance_id().to_owned()).unwrap();
        let app = router(ApiState::new(
            Arc::clone(&store),
            Arc::new(FakeRuntime { instance }),
        ));
        let body = json!({"manifest":manifest()});
        let make = || {
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/applications")
                .header(header::CONTENT_TYPE, JSON)
                .header("idempotency-key", "test-create-1")
                .body(Body::from(body.to_string()))
                .unwrap()
        };
        let first = app.clone().oneshot(make()).await.unwrap();
        let first_status = first.status();
        let first_json = json_body(first).await;
        assert_eq!(first_status, StatusCode::ACCEPTED, "{first_json}");
        let second = app.clone().oneshot(make()).await.unwrap();
        assert_eq!(second.status(), StatusCode::ACCEPTED);
        assert_eq!(first_json, json_body(second).await);
        let listed = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/applications")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            json_body(listed).await["data"]["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn create_retry_returns_original_result_after_later_replacement() {
        let (temp, store, app) = fake_fixture().await;
        let create = || {
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/applications")
                .header(header::CONTENT_TYPE, JSON)
                .header("idempotency-key", "durable-create-retry")
                .body(Body::from(json!({"manifest":manifest()}).to_string()))
                .unwrap()
        };
        let original = json_body(app.clone().oneshot(create()).await.unwrap()).await;
        let id = original["data"]["application_id"].as_str().unwrap();
        let replaced = app
            .clone()
            .oneshot(request(
                Method::PUT,
                &format!("/api/v1/applications/{id}"),
                json!({"expected_generation":1,"manifest":manifest()}),
            ))
            .await
            .unwrap();
        assert_eq!(replaced.status(), StatusCode::ACCEPTED);
        assert_eq!(json_body(replaced).await["data"]["generation"], 2);
        let original_operation = original["data"]["operation_id"].as_str().unwrap();
        store
            .transition_operation(
                original_operation,
                WorkState::Pending,
                WorkState::Running,
                None,
            )
            .await
            .unwrap();
        for step in store.operation_steps(original_operation).await.unwrap() {
            store
                .transition_step(&step.id, StepState::Pending, StepState::Running, None)
                .await
                .unwrap();
            store
                .transition_step(&step.id, StepState::Running, StepState::Succeeded, None)
                .await
                .unwrap();
        }
        store
            .transition_operation(
                original_operation,
                WorkState::Running,
                WorkState::Succeeded,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .prune_finished_operations_before(i64::MAX, 100)
                .await
                .unwrap(),
            0
        );
        drop(app);
        drop(store);
        let reopened = Arc::new(
            SqliteStore::open(temp.path().join("state.db"))
                .await
                .unwrap(),
        );
        let app = router(ApiState::new(reopened, Arc::new(UnavailableRuntime)));
        assert_eq!(
            json_body(app.oneshot(create()).await.unwrap()).await,
            original
        );
    }

    #[tokio::test]
    async fn operation_sse_has_ids_replay_reset_and_terminal_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(
            SqliteStore::open(temp.path().join("state.db"))
                .await
                .unwrap(),
        );
        let instance = InstanceId::parse(store.instance_id().to_owned()).unwrap();
        let app = router(ApiState::new(
            Arc::clone(&store),
            Arc::new(FakeRuntime { instance }),
        ));
        let create = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/applications")
            .header(header::CONTENT_TYPE, JSON)
            .header("idempotency-key", "sse-create")
            .body(Body::from(json!({"manifest":manifest()}).to_string()))
            .unwrap();
        let created = json_body(app.clone().oneshot(create).await.unwrap()).await;
        let operation_id = created["data"]["operation_id"].as_str().unwrap();
        store
            .transition_operation(operation_id, WorkState::Pending, WorkState::Running, None)
            .await
            .unwrap();
        for step in store.operation_steps(operation_id).await.unwrap() {
            store
                .transition_step(&step.id, StepState::Pending, StepState::Running, None)
                .await
                .unwrap();
            store
                .transition_step(&step.id, StepState::Running, StepState::Succeeded, None)
                .await
                .unwrap();
        }
        store
            .transition_operation(operation_id, WorkState::Running, WorkState::Succeeded, None)
            .await
            .unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/operations/{operation_id}/events"))
                    .header("last-event-id", "expired:cursor")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let events = String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(events.contains("event: replay_reset"));
        assert!(events.contains("event: terminal"));
        assert!(events.contains("id:"));
    }

    #[tokio::test]
    async fn operation_current_state_id_changes_when_only_a_step_advances() {
        let (_temp, store, app) = fake_fixture().await;
        let created = json_body(
            app.oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/applications")
                    .header(header::CONTENT_TYPE, JSON)
                    .header("idempotency-key", "step-event-id")
                    .body(Body::from(json!({"manifest":manifest()}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap(),
        )
        .await;
        let operation_id = created["data"]["operation_id"].as_str().unwrap();
        store
            .transition_operation(operation_id, WorkState::Pending, WorkState::Running, None)
            .await
            .unwrap();
        let operation = store.operation(operation_id).await.unwrap();
        let steps = store.operation_steps(operation_id).await.unwrap();
        let before = serde_json::to_string(&operation.clone().view(steps.clone())).unwrap();
        let before_id = current_state_event_id("operation", &before);
        store
            .transition_step(&steps[0].id, StepState::Pending, StepState::Running, None)
            .await
            .unwrap();
        let after = serde_json::to_string(
            &store
                .operation(operation_id)
                .await
                .unwrap()
                .view(store.operation_steps(operation_id).await.unwrap()),
        )
        .unwrap();
        assert_eq!(store.operation(operation_id).await.unwrap(), operation);
        assert_ne!(current_state_event_id("operation", &after), before_id);
    }

    #[tokio::test]
    async fn preview_is_pure_and_unavailable_runtime_is_honest() {
        let (_temp, _store, app) = fixture(Arc::new(UnavailableRuntime)).await;
        let preview = app
            .clone()
            .oneshot(request(
                Method::POST,
                "/api/v1/applications/plan",
                json!({"manifest":manifest()}),
            ))
            .await
            .unwrap();
        assert_eq!(preview.status(), StatusCode::OK);
        assert!(
            json_body(preview).await["data"]["plan"]["actions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["kind"]["action"] == "resolve_image")
        );
        let listed = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/applications")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            json_body(listed).await["data"]["items"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let capabilities = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/system/capabilities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            json_body(capabilities).await["data"]["runtime_execution"],
            false
        );
    }

    #[tokio::test]
    async fn create_preview_never_invokes_resolution_or_observation() {
        let (_temp, _store, app) = fixture(Arc::new(PreviewOnlyRuntime)).await;
        let response = app
            .oneshot(request(
                Method::POST,
                "/api/v1/applications/plan",
                json!({"manifest":manifest()}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            json_body(response).await["data"]["plan"]["actions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|action| action["kind"]["action"] == "resolve_image")
        );
    }

    #[tokio::test]
    async fn replacement_preview_reuses_unchanged_resolutions_without_resolver_io() {
        let (_temp, _store, app) = fake_fixture().await;
        let created = json_body(
            app.clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/api/v1/applications")
                        .header(header::CONTENT_TYPE, JSON)
                        .header("idempotency-key", "preview-reuse")
                        .body(Body::from(json!({"manifest":manifest()}).to_string()))
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        let id = created["data"]["application_id"].as_str().unwrap();
        let preview = app
            .oneshot(request(
                Method::POST,
                &format!("/api/v1/applications/{id}/plan"),
                json!({"expected_generation":1,"manifest":manifest()}),
            ))
            .await
            .unwrap();
        assert_eq!(preview.status(), StatusCode::OK);
        let body = json_body(preview).await;
        let actions = body["data"]["plan"]["actions"].as_array().unwrap();
        assert!(
            actions
                .iter()
                .any(|action| action["mutates_runtime"] == true)
        );
        assert!(
            actions
                .iter()
                .all(|action| action["kind"]["action"] != "resolve_image")
        );
    }

    #[tokio::test]
    async fn transport_failures_are_structured_and_safe() {
        let (_temp, _store, app) = fixture(Arc::new(UnavailableRuntime)).await;
        let missing_key = app
            .clone()
            .oneshot(request(
                Method::POST,
                "/api/v1/applications",
                json!({"manifest":manifest()}),
            ))
            .await
            .unwrap();
        assert_eq!(missing_key.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            json_body(missing_key).await["code"],
            "idempotency_key_required"
        );
        let bad_type = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/applications/plan")
                    .header(header::CONTENT_TYPE, "text/plain")
                    .body(Body::from("secret fixture must not echo"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad_type.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert!(
            !String::from_utf8_lossy(&bad_type.into_body().collect().await.unwrap().to_bytes())
                .contains("secret fixture")
        );
        let unknown = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/secrets")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn validation_not_found_unknown_fields_and_malformed_json_are_structured() {
        let (_temp, _store, app) = fake_fixture().await;
        let invalid = app
            .clone()
            .oneshot(request(
                Method::POST,
                "/api/v1/applications/plan",
                json!({"manifest":{"api_version":"piqueld.dev/v1alpha1","kind":"Application","metadata":{"name":"INVALID"},"spec":{"services":[]}}}),
            ))
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            json_body(invalid).await["code"],
            "manifest_validation_failed"
        );

        let malformed = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/applications/plan")
                    .header(header::CONTENT_TYPE, JSON)
                    .body(Body::from("{\"manifest\":"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_body(malformed).await["code"], "json_malformed");

        let missing = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/applications/app-does-not-exist")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(json_body(missing).await["code"], "not_found");

        let created = json_body(
            app.clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/api/v1/applications")
                        .header(header::CONTENT_TYPE, JSON)
                        .header("idempotency-key", "unknown-field-delete")
                        .body(Body::from(json!({"manifest":manifest()}).to_string()))
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        let id = created["data"]["application_id"].as_str().unwrap();
        let unknown_field = app
            .oneshot(request(
                Method::DELETE,
                &format!("/api/v1/applications/{id}"),
                json!({"expected_generation":1,"force":true}),
            ))
            .await
            .unwrap();
        assert_eq!(unknown_field.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_body(unknown_field).await["code"], "json_malformed");
    }

    #[tokio::test]
    async fn openapi_snapshot_lists_exact_plan_five_surface() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/openapi-v1.json");
        if !path.exists() {
            // Nix's Git-backed source excludes untracked files while developing a
            // new plan. The normal workspace test below remains strict once the
            // snapshot enters the branch.
            return;
        }
        let snapshot: Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(openapi::openapi_document(), snapshot);

        let (_temp, _store, app) = fixture(Arc::new(UnavailableRuntime)).await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/vnd.oai.openapi+json;version=3.1"
        );
        assert_eq!(json_body(response).await, snapshot);

        assert!(snapshot["paths"].get("/api/v1/secrets").is_none());
        assert!(
            snapshot["paths"]
                .get("/api/v1/applications/{id}/logs")
                .is_none()
        );
        for item in snapshot["paths"].as_object().unwrap().values() {
            for operation in item.as_object().unwrap().values() {
                assert!(operation.get("operationId").is_some());
                assert!(operation.get("responses").is_some());
            }
        }
        assert!(snapshot["components"]["schemas"].as_object().unwrap().len() > 20);
        assert_refs_resolve(
            &snapshot,
            snapshot["components"]["schemas"].as_object().unwrap(),
        );
    }

    #[tokio::test]
    async fn request_ids_match_headers_and_transport_errors_are_complete() {
        let (_temp, _store, app) = fixture(Arc::new(UnavailableRuntime)).await;
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::PATCH)
                    .uri("/api/v1/applications")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        let request_id = response
            .headers()
            .get("x-request-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let body = json_body(response).await;
        assert_eq!(body["code"], "method_not_allowed");
        assert_eq!(body["request_id"], request_id);
    }

    #[tokio::test]
    async fn json_and_toml_share_normalization_and_malformed_toml_is_safe() {
        let (_temp, _store, app) = fake_fixture().await;
        let json_plan = app
            .clone()
            .oneshot(request(
                Method::POST,
                "/api/v1/applications/plan",
                json!({"manifest":manifest()}),
            ))
            .await
            .unwrap();
        let toml = r#"api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "notes"
[[spec.services]]
name = "web"
[spec.services.source]
type = "image"
image = "ghcr.io/example/notes:1"
"#;
        let toml_plan = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/applications/plan")
                    .header(header::CONTENT_TYPE, TOML)
                    .body(Body::from(toml))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(json_plan.status(), StatusCode::OK);
        assert_eq!(toml_plan.status(), StatusCode::OK);
        assert_eq!(json_body(json_plan).await, json_body(toml_plan).await);

        let malformed = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/applications/plan")
                    .header(header::CONTENT_TYPE, TOML)
                    .body(Body::from("[metadata\nsecret = 'must-not-echo'"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_body(malformed).await["code"], "toml_malformed");
    }

    #[tokio::test]
    async fn conflicts_and_reconcile_retries_preserve_durable_identity() {
        let (_temp, _store, app) = fake_fixture().await;
        let create = |key: &'static str| {
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/applications")
                .header(header::CONTENT_TYPE, JSON)
                .header("idempotency-key", key)
                .body(Body::from(json!({"manifest":manifest()}).to_string()))
                .unwrap()
        };
        let created = json_body(app.clone().oneshot(create("first")).await.unwrap()).await;
        let id = created["data"]["application_id"].as_str().unwrap();
        let collision = app.clone().oneshot(create("second")).await.unwrap();
        assert_eq!(collision.status(), StatusCode::CONFLICT);
        assert_eq!(
            json_body(collision).await["code"],
            "application_name_collision"
        );
        let stale = app
            .clone()
            .oneshot(request(
                Method::PUT,
                &format!("/api/v1/applications/{id}"),
                json!({"expected_generation":2,"manifest":manifest()}),
            ))
            .await
            .unwrap();
        assert_eq!(stale.status(), StatusCode::CONFLICT);
        assert_eq!(json_body(stale).await["details"]["current_generation"], 1);

        let reconcile = || {
            request(
                Method::POST,
                &format!("/api/v1/applications/{id}/reconcile"),
                json!({"expected_generation":1}),
            )
        };
        let first = json_body(app.clone().oneshot(reconcile()).await.unwrap()).await;
        let second = json_body(app.clone().oneshot(reconcile()).await.unwrap()).await;
        assert_eq!(first, second);
        let operation_id = first["data"]["operation_id"].as_str().unwrap();
        let operation = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/operations/{operation_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(operation.status(), StatusCode::OK);
        assert_eq!(json_body(operation).await["data"]["kind"], "reconcile");
    }

    #[tokio::test]
    async fn concurrent_create_retries_and_name_collisions_have_one_durable_winner() {
        let (_temp, _store, app) = fake_fixture().await;
        let create = |key: &'static str| {
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/applications")
                .header(header::CONTENT_TYPE, JSON)
                .header("idempotency-key", key)
                .body(Body::from(json!({"manifest":manifest()}).to_string()))
                .unwrap()
        };
        let (same_left, same_right) = tokio::join!(
            app.clone().oneshot(create("same-race")),
            app.clone().oneshot(create("same-race"))
        );
        let same_left = same_left.unwrap();
        let same_right = same_right.unwrap();
        assert_eq!(same_left.status(), StatusCode::ACCEPTED);
        assert_eq!(same_right.status(), StatusCode::ACCEPTED);
        assert_eq!(json_body(same_left).await, json_body(same_right).await);

        let second_manifest = json!({
            "api_version":"piqueld.dev/v1alpha1",
            "kind":"Application",
            "metadata":{"name":"other"},
            "spec":{"services":[{"name":"web","source":{"type":"image","image":"ghcr.io/example/notes:1"}}]}
        });
        let conflicting_key = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/applications")
            .header(header::CONTENT_TYPE, JSON)
            .header("idempotency-key", "same-race")
            .body(Body::from(json!({"manifest":second_manifest}).to_string()))
            .unwrap();
        let reused = app.clone().oneshot(conflicting_key).await.unwrap();
        assert_eq!(reused.status(), StatusCode::CONFLICT);
        assert_eq!(json_body(reused).await["code"], "idempotency_key_reused");

        let (_collision_temp, _collision_store, collision_app) = fake_fixture().await;
        let (collision_left, collision_right) = tokio::join!(
            collision_app.clone().oneshot(create("collision-left")),
            collision_app.clone().oneshot(create("collision-right"))
        );
        let statuses = [
            collision_left.unwrap().status(),
            collision_right.unwrap().status(),
        ];
        assert!(statuses.contains(&StatusCode::ACCEPTED));
        assert!(statuses.contains(&StatusCode::CONFLICT));
        assert_eq!(
            json_body(
                collision_app
                    .oneshot(
                        Request::builder()
                            .uri("/api/v1/applications")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap()
            )
            .await["data"]["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn active_reconcile_retry_does_not_repeat_runtime_io() {
        let (_temp, store, app) = fake_fixture().await;
        let created = json_body(
            app.oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/applications")
                    .header(header::CONTENT_TYPE, JSON)
                    .header("idempotency-key", "reconcile-lost-response")
                    .body(Body::from(json!({"manifest":manifest()}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap(),
        )
        .await;
        let id = ApplicationId::parse(created["data"]["application_id"].as_str().unwrap()).unwrap();
        let durable = store
            .request_reconcile(&id, 1, &["already durable".into()])
            .await
            .unwrap();
        let retry_router = router(ApiState::new(store, Arc::new(ActiveRetryRuntime)));
        let response = retry_router
            .oneshot(request(
                Method::POST,
                &format!("/api/v1/applications/{id}/reconcile"),
                json!({"expected_generation":1}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            json_body(response).await["data"]["operation_id"],
            durable.operation_id
        );
    }
}
