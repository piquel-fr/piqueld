//! Versioned HTTP/JSON API boundary.

use async_trait::async_trait;
use axum::{
    Extension, Router,
    body::Body,
    extract::Request,
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use piqueld_client::{
    AcceptedOperation, CreateApplicationRequest, Envelope, ErrorBody, PlanApplicationRequest,
    ReplaceApplicationRequest,
};
use piqueld_core::{
    ApplicationId, ApplicationIdError, CompileError, NormalizedApplication, ObservedApplication,
    resource::ResolvedApplication,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, RequestId, SetRequestIdLayer},
    trace::TraceLayer,
};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    docker::DockerError,
    store::{SqliteStore, StoreError, StoredApplication},
};

mod applications;
mod openapi;
mod operations;
mod system;

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
}

/// Source resolution, runtime observation, and execution seam supplied by Plan 06.
#[async_trait]
pub trait RuntimeBoundary: Send + Sync + 'static {
    /// Wakes the reconciler after a mutation requests an immediate scan.
    fn trigger_reconciliation(&self) {}
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
}

/// Shared state for API handlers.
#[derive(Clone)]
pub struct ApiState {
    store: Arc<SqliteStore>,
    runtime: Arc<dyn RuntimeBoundary>,
    instance_id: String,
    mutation_lock: Arc<tokio::sync::Mutex<()>>,
}

impl ApiState {
    /// Creates API state backed by the given store and runtime adapter.
    #[must_use]
    pub fn new(store: Arc<SqliteStore>, runtime: Arc<dyn RuntimeBoundary>) -> Self {
        Self {
            instance_id: store.instance_id().to_owned(),
            store,
            runtime,
            mutation_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Serializes mutation preparation so the idempotency lookup-through-commit
    /// window cannot race with a concurrent retry of the same key.
    pub(super) async fn mutation_guard(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.mutation_lock.lock().await
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    details: Value,
    allow: Option<String>,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
            details: Value::Null,
            allow: None,
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
        let mut response = (
            self.status,
            [(header::CONTENT_TYPE, JSON)],
            axum::Json(body),
        )
            .into_response();
        if let Some(allow) = self.allow
            && let Ok(value) = header::HeaderValue::from_str(&allow)
        {
            response.headers_mut().insert(header::ALLOW, value);
        }
        response
    }
}

/// Builds the Plan 06 HTTP router.
pub fn router(state: ApiState) -> Router {
    let request_id = header::HeaderName::from_static("x-request-id");
    let (router, openapi) = documented_router().split_for_parts();
    router
        .method_not_allowed_fallback(|| async { method_not_allowed().await })
        .fallback(fallback)
        .with_state(state)
        .layer(Extension(Arc::new(openapi)))
        .layer(PropagateRequestIdLayer::new(request_id.clone()))
        .layer(middleware::from_fn(bind_error_request_id))
        .layer(middleware::from_fn(host_allowlist))
        .layer(SetRequestIdLayer::new(request_id, MakeRequestUuid))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|request: &Request| {
                    tracing::info_span!(
                        "http_request",
                        method = %request.method(),
                        path = %request.uri().path(),
                    )
                })
                .on_response(|response: &Response, latency: std::time::Duration, _: &tracing::Span| {
                    tracing::info!(status = %response.status(), latency_ms = u64::try_from(latency.as_millis()).unwrap_or(u64::MAX), "request completed");
                }),
        )
}

/// The unauthenticated control plane only accepts loopback-style authorities.
/// Browsers reaching any other Host would indicate DNS rebinding.
async fn host_allowlist(request: Request, next: Next) -> Response {
    let allowed = request
        .headers()
        .get(header::HOST)
        .is_none_or(|value| value.to_str().is_ok_and(allowed_host));
    if allowed {
        next.run(request).await
    } else {
        ApiError::new(
            StatusCode::FORBIDDEN,
            "host_not_allowed",
            "request host is not permitted",
        )
        .into_response()
    }
}

fn allowed_host(raw: &str) -> bool {
    let authority = match raw.rsplit_once(':') {
        Some((head, tail))
            if !tail.is_empty() && tail.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            head
        }
        _ => raw,
    };
    let lowered = authority.to_ascii_lowercase();
    let unbracketed = lowered
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(&lowered);
    matches!(unbracketed, "localhost" | "127.0.0.1" | "::1")
}

// Public endpoints must be registered through `routes!` here so Axum and the
// generated OpenAPI document receive the same method and path at the same time.
fn documented_router() -> OpenApiRouter<ApiState> {
    OpenApiRouter::with_openapi(openapi::base_document())
        .routes(routes!(system::status))
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
        .routes(routes!(operations::get))
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

async fn fallback() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "endpoint_not_found",
        "API endpoint was not found",
    )
}
#[allow(clippy::unused_async)]
async fn method_not_allowed() -> ApiError {
    let mut error = ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "HTTP method is not allowed",
    );
    error.allow = Some("GET, POST, PATCH, DELETE".into());
    error
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
    let mut values = headers.get_all("idempotency-key").iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    let invalid = || {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "idempotency_key_invalid",
            "Idempotency-Key is invalid",
        )
    };
    if values.next().is_some() {
        return Err(invalid());
    }
    let Ok(key) = value.to_str() else {
        return Err(invalid());
    };
    if key.is_empty() || key.len() > 128 || key.chars().any(char::is_control) {
        return Err(invalid());
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
    let malformed = || {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "json_malformed",
            "request JSON is malformed or contains unknown fields",
        )
    };
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    let value = serde_path_to_error::deserialize(&mut deserializer).map_err(|error| {
        tracing::debug!(path = %error.path(), "request JSON was rejected");
        malformed()
    })?;
    deserializer.end().map_err(|_| malformed())?;
    Ok(value)
}

fn parse_expected_generation(raw: &str) -> Result<u64, ()> {
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(());
    }
    raw.parse().map_err(|_| ())
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
                })
                .and_then(|raw| {
                    parse_expected_generation(raw).map_err(|()| {
                        ApiError::new(
                            StatusCode::BAD_REQUEST,
                            "expected_generation_invalid",
                            "expected generation is invalid",
                        )
                    })
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
