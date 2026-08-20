//! Versioned HTTP/JSON API boundary.

use async_trait::async_trait;
use axum::{
    Extension, Router,
    body::Body,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response, sse::Event},
};
use futures_util::{Stream, stream};
use piqueld_client::{
    AcceptedOperation, ApplicationLogsOptions, ContainerLogView, CreateApplicationRequest,
    Envelope, ErrorBody, PlanApplicationRequest, ReplaceApplicationRequest, ServiceStatusView,
};
use piqueld_core::{
    ApplicationId, ApplicationIdError, CompileError, NormalizedApplication, ObservedApplication,
    resource::ResolvedApplication,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{convert::Infallible, future::Future, pin::Pin, sync::Arc, time::Duration};
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, RequestId, SetRequestIdLayer},
    trace::TraceLayer,
};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    auth::AccessPolicy,
    build::{BuildError, BuildLogEntry},
    config::{SecurityConfig, default_ui_dir},
    docker::DockerError,
    proxy::IngressError,
    secrets::{SecretError, SecretService},
    store::{SqliteStore, StoreError, StoredApplication},
    transfer::{StateTransferService, TransferError},
};

mod applications;
mod builds;
mod openapi;
mod operations;
mod secrets;
mod system;
mod transfer;
mod ui;

pub use openapi::openapi_document;

const JSON: &str = "application/json";
const TOML: &str = "application/toml";

#[derive(Clone, Debug)]
/// Resolved desired state paired with the initial runtime observation.
pub struct PreparedApplication {
    /// Immutable desired application state.
    pub resolved: ResolvedApplication,
    /// Runtime resources observed before planning.
    pub observed: ObservedApplication,
    /// Build outputs prepared before the mutation was journaled.
    pub builds: Vec<PreparedBuild>,
}

/// Registry-verified build output ready to be attached to a durable operation.
#[derive(Clone, Debug)]
pub struct PreparedBuild {
    /// Logical service name.
    pub service_name: String,
    /// Resolved source commit.
    pub source_commit: String,
    /// Mutable registry tag.
    pub image_reference: String,
    /// Immutable registry digest reference.
    pub image_digest: String,
    /// Canonical build identity hash.
    pub build_key: String,
    /// Deterministic context archive hash.
    pub context_hash: String,
    /// Redacted bounded build logs.
    pub logs: Vec<BuildLogEntry>,
}

#[derive(Debug, thiserror::Error)]
/// Errors crossing the runtime boundary.
pub enum BoundaryError {
    /// A Docker runtime request failed.
    #[error("runtime request failed")]
    Runtime(#[from] DockerError),
    /// Resolved inputs could not be compiled into desired runtime resources.
    #[error("application compilation failed")]
    Compilation(Vec<CompileError>),
    /// Logical-secret resolution failed.
    #[error("logical-secret resolution failed")]
    Secrets(#[from] SecretError),
    /// Git source preparation or registry verification failed.
    #[error("source build failed")]
    Build(#[from] BuildError),
    /// Managed ingress infrastructure failed to converge.
    #[error("ingress infrastructure failed")]
    Ingress(#[from] IngressError),
}

/// Source resolution, runtime observation, and execution seam supplied by Plan 06.
#[async_trait]
pub trait RuntimeBoundary: Send + Sync + 'static {
    /// Wakes the reconciler after a mutation requests an immediate scan.
    fn trigger_reconciliation(&self) {}
    /// Checks runtime and managed-infrastructure readiness without creating
    /// resources or exposing lower-level dependency errors.
    async fn readiness(&self) -> RuntimeReadiness {
        RuntimeReadiness::unavailable()
    }
    /// Resolves mutable inputs and captures an initial runtime observation.
    async fn prepare(
        &self,
        application: &NormalizedApplication,
    ) -> Result<PreparedApplication, BoundaryError>;
    /// Captures current runtime state for a stored application.
    async fn observe(
        &self,
        application: &StoredApplication,
    ) -> Result<ObservedApplication, BoundaryError>;
    /// Returns bounded service convergence status for a stored application.
    async fn runtime_status(
        &self,
        _application: &StoredApplication,
    ) -> Result<Vec<ServiceStatusView>, BoundaryError> {
        Ok(Vec::new())
    }
    /// Returns managed infrastructure status for an application, when relevant.
    async fn infrastructure_status(
        &self,
        _application: &StoredApplication,
    ) -> Result<Option<String>, BoundaryError> {
        Ok(None)
    }
    /// Returns bounded, presentation-safe application logs.
    async fn application_logs(
        &self,
        _application: &StoredApplication,
        _options: &ApplicationLogsOptions,
    ) -> Result<Vec<ContainerLogView>, BoundaryError> {
        Ok(Vec::new())
    }
}

#[derive(Clone, Debug)]
/// Sanitized readiness state returned by the runtime boundary.
pub struct RuntimeReadiness {
    /// Whether Docker is reachable.
    pub docker: bool,
    /// Whether Docker is an active single-node Swarm manager.
    pub swarm_manager: bool,
    /// Whether registry and ingress dependencies are ready.
    pub infrastructure: bool,
    /// Sanitized operator-facing reason when readiness is false.
    pub reason: Option<String>,
}

impl RuntimeReadiness {
    fn unavailable() -> Self {
        Self {
            docker: false,
            swarm_manager: false,
            infrastructure: false,
            reason: Some("runtime dependencies are unavailable".into()),
        }
    }
}

/// Shared state for API handlers.
#[derive(Clone)]
pub struct ApiState {
    store: Arc<SqliteStore>,
    runtime: Arc<dyn RuntimeBoundary>,
    secret_service: Option<Arc<SecretService>>,
    instance_id: String,
    create_lock: Arc<tokio::sync::Mutex<()>>,
    transfer: StateTransferService,
    import_confirmations:
        Arc<tokio::sync::Mutex<std::collections::BTreeMap<String, (String, i64)>>>,
    import_lock: Arc<tokio::sync::Mutex<()>>,
    ui_dir: std::path::PathBuf,
    metrics_enabled: bool,
}

impl ApiState {
    /// Creates API state backed by the given store and runtime adapter.
    #[must_use]
    pub fn new(store: &Arc<SqliteStore>, runtime: Arc<dyn RuntimeBoundary>) -> Self {
        Self {
            instance_id: store.instance_id().to_owned(),
            store: Arc::clone(store),
            runtime,
            secret_service: None,
            create_lock: Arc::new(tokio::sync::Mutex::new(())),
            transfer: StateTransferService::new(Arc::clone(store)),
            import_confirmations: Arc::new(tokio::sync::Mutex::new(
                std::collections::BTreeMap::new(),
            )),
            import_lock: Arc::new(tokio::sync::Mutex::new(())),
            ui_dir: default_ui_dir(),
            metrics_enabled: false,
        }
    }

    /// Selects the production asset directory used by the loopback web listener.
    #[must_use]
    pub fn with_ui_dir(mut self, path: std::path::PathBuf) -> Self {
        self.ui_dir = path;
        self
    }

    /// Enables the low-cardinality Prometheus endpoint.
    #[must_use]
    pub fn with_metrics(mut self, enabled: bool) -> Self {
        self.metrics_enabled = enabled;
        self
    }

    /// Enables logical-secret API operations.
    #[must_use]
    pub fn with_secret_service(mut self, service: Arc<SecretService>) -> Self {
        self.transfer.set_secret_service(Arc::clone(&service));
        self.secret_service = Some(service);
        self
    }

    fn ordinary_lease(&self) -> Result<tokio::sync::OwnedRwLockReadGuard<()>, ApiError> {
        let gate = self.store.maintenance_gate();
        gate.try_read_owned().map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "state_import_maintenance",
                "ordinary mutations and reconciliation are paused for state import",
            )
        })
    }

    pub(crate) fn secret_service(&self) -> Option<&Arc<SecretService>> {
        self.secret_service.as_ref()
    }

    pub(crate) fn ui_dir(&self) -> &std::path::Path {
        &self.ui_dir
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
        if matches!(
            &value,
            StoreError::Database
                | StoreError::DatabaseSource(_)
                | StoreError::SchemaMismatch
                | StoreError::SchemaMismatchSource(_)
                | StoreError::PathSource(_)
                | StoreError::Corrupt
                | StoreError::CorruptSource(_)
        ) {
            tracing::error!(error = ?value, "storage request failed");
        }
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
            StoreError::IllegalTransition => Self::new(
                StatusCode::CONFLICT,
                "application_state_conflict",
                "the requested application transition is not allowed",
            ),
            StoreError::InvalidInput | StoreError::InvalidInputSource(_) => Self::new(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "the request is invalid",
            ),
            StoreError::SchemaMismatch | StoreError::SchemaMismatchSource(_) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "schema_mismatch",
                "database schema is incompatible",
            ),
            StoreError::Corrupt | StoreError::CorruptSource(_) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "stored_state_corrupt",
                "stored application state is corrupt",
            ),
            StoreError::Database | StoreError::DatabaseSource(_) | StoreError::PathSource(_) => {
                Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "storage_unavailable",
                    "control-plane storage is unavailable",
                )
            }
        }
    }
}

impl From<BoundaryError> for ApiError {
    fn from(value: BoundaryError) -> Self {
        tracing::error!(error = ?value, "runtime boundary request failed");
        match value {
            BoundaryError::Runtime(_) => Self::new(
                StatusCode::BAD_GATEWAY,
                "runtime_request_failed",
                "runtime request failed",
            ),
            BoundaryError::Compilation(_) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "application_compilation_failed",
                "application compilation failed",
            ),
            BoundaryError::Build(_) => Self::new(
                StatusCode::BAD_GATEWAY,
                "runtime_request_failed",
                "runtime request failed",
            ),
            BoundaryError::Ingress(_) => Self::new(
                StatusCode::BAD_GATEWAY,
                "runtime_request_failed",
                "runtime request failed",
            ),
            BoundaryError::Secrets(error) => error.into(),
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

impl From<SecretError> for ApiError {
    fn from(value: SecretError) -> Self {
        match value {
            SecretError::InvalidName | SecretError::InvalidValue => Self::new(
                StatusCode::BAD_REQUEST,
                "secret_invalid",
                "secret name or value is invalid",
            ),
            SecretError::InvalidPagination => Self::new(
                StatusCode::BAD_REQUEST,
                "pagination_invalid",
                "pagination parameters are invalid",
            ),
            SecretError::NotFound => Self::new(
                StatusCode::NOT_FOUND,
                "secret_not_found",
                "logical secret was not found",
            ),
            SecretError::AlreadyExists => Self::new(
                StatusCode::CONFLICT,
                "secret_exists",
                "logical secret already exists",
            ),
            SecretError::Referenced => Self::new(
                StatusCode::CONFLICT,
                "secret_referenced",
                "logical secret is still referenced by desired or deployed state",
            ),
            SecretError::KeyUnavailable | SecretError::KeyPermissions | SecretError::KeyInvalid => {
                Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "master_key_unavailable",
                    "master encryption key is unavailable",
                )
            }
            SecretError::DecryptionFailed => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "secret_decryption_failed",
                "logical secret could not be decrypted",
            ),
            SecretError::Storage => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "secret_storage_unavailable",
                "secret storage is unavailable",
            ),
        }
    }
}

impl From<TransferError> for ApiError {
    fn from(value: TransferError) -> Self {
        match value {
            TransferError::Malformed => Self::new(
                StatusCode::BAD_REQUEST,
                "archive_malformed",
                "state archive is malformed",
            ),
            TransferError::Limit => Self::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "archive_limit",
                "state archive exceeds a safety limit",
            ),
            TransferError::Checksum => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "archive_checksum",
                "state archive checksum verification failed",
            ),
            TransferError::Schema => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "archive_schema",
                "state archive schema is unsupported",
            ),
            TransferError::Validation => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "archive_validation",
                "state archive failed domain validation",
            ),
            TransferError::Confirmation => Self::new(
                StatusCode::PRECONDITION_FAILED,
                "replace_confirmation_invalid",
                "replace confirmation is missing, expired, or does not match this archive",
            ),
            TransferError::Maintenance => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "state_import_maintenance",
                "state import maintenance is already active",
            ),
            TransferError::Storage => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "state_transfer_failed",
                "state transfer storage failed",
            ),
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

/// Builds the Plan 06 HTTP router.
pub fn router(state: ApiState) -> Router {
    let security = SecurityConfig::default();
    router_with_policy(
        state,
        AccessPolicy::unix(&security),
        security.max_body_bytes,
    )
}

/// Builds the API-only router used by the Unix-socket client transport.
pub fn api_router(state: ApiState) -> Router {
    let security = SecurityConfig::default();
    api_router_with_policy(
        state,
        AccessPolicy::unix(&security),
        security.max_body_bytes,
    )
}

/// Builds the browser/API router with an explicit listener security policy.
pub fn router_with_policy(state: ApiState, policy: AccessPolicy, max_body_bytes: usize) -> Router {
    build_router(state, policy, max_body_bytes, true)
}

/// Builds the API-only router with an explicit listener security policy.
pub fn api_router_with_policy(
    state: ApiState,
    policy: AccessPolicy,
    max_body_bytes: usize,
) -> Router {
    build_router(state, policy, max_body_bytes, false)
}

fn build_router(
    state: ApiState,
    policy: AccessPolicy,
    max_body_bytes: usize,
    serve_ui: bool,
) -> Router {
    let request_id = header::HeaderName::from_static("x-request-id");
    let (router, openapi) = documented_router().split_for_parts();
    let router = router.method_not_allowed_fallback(method_not_allowed);
    let router = if serve_ui {
        router.fallback(fallback)
    } else {
        router.fallback(api_fallback)
    };
    router
        .with_state(state)
        .layer(DefaultBodyLimit::max(
            max_body_bytes.min(crate::transfer::MAX_ARCHIVE_BYTES),
        ))
        .layer(Extension(Arc::new(openapi)))
        .layer(PropagateRequestIdLayer::new(request_id.clone()))
        .layer(middleware::from_fn(bind_error_request_id))
        .layer(SetRequestIdLayer::new(request_id, MakeRequestUuid))
        // Never record the raw URI: query strings are operator input and can
        // accidentally contain credentials or archive confirmation material.
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &Request| {
                tracing::info_span!(
                    "http_request",
                    method = %request.method(),
                    request_id = request
                        .headers()
                        .get("x-request-id")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("unavailable")
                )
            }),
        )
        .layer(middleware::from_fn_with_state(policy, crate::auth::enforce))
}

// Public endpoints must be registered through `routes!` here so Axum and the
// generated OpenAPI document receive the same method and path at the same time.
fn documented_router() -> OpenApiRouter<ApiState> {
    OpenApiRouter::with_openapi(openapi::base_document())
        .routes(routes!(system::status))
        .routes(routes!(system::capabilities))
        .routes(routes!(system::health))
        .routes(routes!(system::health_v1))
        .routes(routes!(system::readiness))
        .routes(routes!(system::metrics))
        .routes(routes!(openapi::openapi))
        .routes(routes!(applications::list, applications::create))
        .routes(routes!(applications::plan_create))
        .routes(routes!(
            applications::get,
            applications::replace,
            applications::delete
        ))
        .routes(routes!(applications::detail))
        .routes(routes!(applications::plan_replace))
        .routes(routes!(applications::plan_delete))
        .routes(routes!(applications::reconcile))
        .routes(routes!(applications::status))
        .routes(routes!(applications::logs))
        .routes(routes!(applications::events))
        .routes(routes!(
            transfer::export_state,
            transfer::prepare_state_import
        ))
        .routes(routes!(
            transfer::import_state,
            transfer::export_application
        ))
        .routes(routes!(operations::get))
        .routes(routes!(operations::events))
        .routes(routes!(builds::get))
        .routes(routes!(builds::operation_builds))
        .routes(routes!(builds::logs))
        .routes(routes!(secrets::list, secrets::create_header))
        .routes(routes!(
            secrets::get,
            secrets::create,
            secrets::replace,
            secrets::delete
        ))
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

async fn fallback(State(state): State<ApiState>, request: Request) -> Response {
    if ui::is_reserved_path(request.uri().path()) {
        return api_fallback(request.method().clone()).await.into_response();
    }
    ui::fallback(state.ui_dir().to_owned(), request).await
}

async fn api_fallback(method: Method) -> ApiError {
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

fn optional_idempotency_key(headers: &HeaderMap) -> Result<Option<&str>, ApiError> {
    let Some(key) = header_text(headers, "idempotency-key") else {
        return Ok(None);
    };
    if key.is_empty() || key.len() > 128 || key.chars().any(char::is_control) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "idempotency_key_invalid",
            "Idempotency-Key is invalid",
        ));
    }
    Ok(Some(key))
}

fn mutation_request_hash(
    kind: &str,
    application_id: &ApplicationId,
    expected_generation: u64,
    spec_hash: Option<&str>,
) -> String {
    let request = format!(
        "piqueld-mutation/v1\0{kind}\0{application_id}\0{expected_generation}\0{}",
        spec_hash.unwrap_or_default()
    );
    format!("sha256:{}", hex(&Sha256::digest(request.as_bytes())))
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
fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        value.push(char::from(HEX[usize::from(byte >> 4)]));
        value.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    value
}

pub(crate) type EventStream = Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>;

pub(crate) struct StateEventSnapshot {
    pub(crate) data: String,
    pub(crate) event: &'static str,
    pub(crate) terminal: bool,
}

/// Polls one durable/current-state snapshot and emits it only when its cursor
/// changes. Both status and log streams use this bounded replay model.
pub(crate) fn current_state_stream<F, Fut>(
    kind: &'static str,
    last: Option<String>,
    fetch: F,
) -> EventStream
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = Option<StateEventSnapshot>> + Send,
{
    Box::pin(stream::unfold(
        (fetch, last, false),
        move |(fetch, last, done)| async move {
            if done {
                return None;
            }
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let snapshot = fetch().await?;
                let event_id = current_state_event_id(kind, &snapshot.data);
                if last.as_deref() == Some(event_id.as_str()) {
                    continue;
                }
                let terminal = snapshot.terminal;
                let event = Event::default()
                    .id(event_id.clone())
                    .event(snapshot.event)
                    .data(snapshot.data);
                return Some((Ok(event), (fetch, Some(event_id), terminal)));
            }
        },
    ))
}

pub(crate) fn current_state_event_id(kind: &str, data: &str) -> String {
    let digest = Sha256::digest(format!("piqueld-sse/v1\0{kind}\0{data}").as_bytes());
    format!("{kind}-{}", hex(&digest[..16]))
}

pub(crate) fn last_event_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= 128 && !value.contains('\0'))
        .map(str::to_owned)
}
