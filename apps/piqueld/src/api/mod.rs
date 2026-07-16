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
use futures_util::{Stream, stream};
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
use std::{convert::Infallible, future::Future, pin::Pin, sync::Arc, time::Duration};
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, RequestId, SetRequestIdLayer},
    trace::TraceLayer,
};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::store::{SqliteStore, StoreError, StoredApplication};

#[cfg(test)]
mod tests;

mod applications;
mod openapi;
mod operations;
mod system;

pub use openapi::openapi_document;

const JSON: &str = "application/json";
const TOML: &str = "application/toml";
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

struct StateEventSnapshot {
    data: String,
    event: &'static str,
    terminal: bool,
}

fn current_state_stream<F, Fut>(kind: &'static str, last: Option<String>, fetch: F) -> EventStream
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = Option<StateEventSnapshot>> + Send,
{
    Box::pin(stream::unfold(
        (fetch, last, false, true),
        move |(fetch, last, done, reconnect)| async move {
            if done {
                return None;
            }
            loop {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let snapshot = fetch().await?;
                let event_id = current_state_event_id(kind, &snapshot.data);
                if last.as_deref() == Some(event_id.as_str()) {
                    if snapshot.terminal {
                        return None;
                    }
                    continue;
                }
                if reconnect && last.is_some() {
                    let reset = Event::default()
                        .id(format!("reset:{event_id}"))
                        .event("replay_reset")
                        .data("{\"reason\":\"bounded_replay_exhausted\"}");
                    return Some((Ok(reset), (fetch, None, false, false)));
                }
                let event = Event::default()
                    .id(event_id.clone())
                    .event(snapshot.event)
                    .data(snapshot.data);
                return Some((Ok(event), (fetch, Some(event_id), snapshot.terminal, false)));
            }
        },
    ))
}

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
