//! Versioned HTTP/JSON API boundary.

use async_trait::async_trait;
use axum::{
    Extension, Router,
    body::Body,
    extract::Request,
    http::{HeaderMap, Method, StatusCode, header},
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
    create_lock: Arc<tokio::sync::Mutex<()>>,
}

impl ApiState {
    /// Creates API state backed by the given store and runtime adapter.
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
    /// Maps a storage error to its corresponding HTTP API error.
    ///
    /// # Examples
    ///
    /// ```
    /// let error: ApiError = StoreError::NotFound.into();
    /// ```
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
    /// Converts a runtime boundary failure into an HTTP API error.
    ///
    /// Runtime failures map to a `502 Bad Gateway` response, while application
    /// compilation failures map to a `500 Internal Server Error` response.
    ///
    /// # Examples
    ///
    /// ```
    /// let error: ApiError = BoundaryError::Compilation(vec![]).into();
    /// let _ = error;
    /// ```
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
    /// Converts the API error into a JSON HTTP response with its status, error details, and a generated request ID.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn example(error: ApiError) {
    /// let response = error.into_response();
    /// assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    /// # }
    /// ```
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

/// Builds the HTTP router with documented routes, standardized fallbacks, request-ID handling, and tracing.
///
/// # Examples
///
/// ```
/// # let state = todo!();
/// let app = router(state);
/// ```
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
/// Builds the OpenAPI-aware router for system, application, planning, reconciliation, status, and operation endpoints.
///
/// # Examples
///
/// ```
/// let router = documented_router();
/// ```
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

/// Binds the incoming request ID to JSON error responses while preserving other responses.
///
/// # Examples
///
/// ```no_run
/// let app = axum::Router::new()
///     .layer(axum::middleware::from_fn(bind_error_request_id));
/// ```
async fn bind_error_request_id(request: Request, next: Next) -> Response {
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

/// Creates the standard error for requests to an unknown endpoint.
///
/// # Examples
///
/// ```
/// # async fn example() {
/// let _error = fallback(Method::GET).await;
/// # }
/// ```
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
/// Produces a deterministic SHA-256 hash for an idempotency key.
///
/// # Examples
///
/// ```
/// let hash = idempotency_key_hash("request-123");
/// assert!(hash.starts_with("sha256:"));
/// ```
fn idempotency_key_hash(key: &str) -> String {
    format!("sha256:{}", hex(&Sha256::digest(key.as_bytes())))
}
/// Converts bytes to lowercase hexadecimal text.
///
/// # Examples
///
/// ```
/// assert_eq!(hex(&[0x0a, 0xff]), "0aff");
/// ```
///
/// # Returns
///
/// A lowercase hexadecimal representation with two characters per byte.
fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        value.push(char::from(HEX[usize::from(byte >> 4)]));
        value.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    value
}
