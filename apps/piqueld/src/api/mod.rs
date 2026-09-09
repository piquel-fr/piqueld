//! Versioned HTTP/JSON API boundary.

use axum::{
    Extension, Router,
    body::Body,
    extract::{DefaultBodyLimit, Request},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use piqueld_core::ApplicationIdError;
use piqueld_core::api::{AcceptedOperation, ApplyApplicationRequest, Envelope, ErrorBody};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, RequestId, SetRequestIdLayer},
    trace::TraceLayer,
};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::store::StoreError;

mod applications;
mod events;
mod openapi;
mod operations;
mod system;
mod ui;

pub use crate::application::Applications as ApiState;
use crate::application::{ApplicationError, BoundaryError};
pub use openapi::openapi_document;
pub use ui::{EmbeddedBundle, UiAssets};

const JSON: &str = "application/json";
const TOML: &str = "application/toml";

/// Upper bound for one API request body. The CLI's manifest preflight limit
/// (`piquelctl::support::MAX_MANIFEST_BYTES`) must not exceed this value, or a
/// locally accepted manifest would fail server-side with 413.
const REQUEST_BODY_LIMIT_BYTES: usize = 2 * 1024 * 1024;

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
            StoreError::GenerationConflict { expected, actual } => Self::new(
                StatusCode::CONFLICT,
                "generation_conflict",
                "application intent changed",
            )
            .details(
                serde_json::json!({"expected_generation":expected,"actual_generation":actual}),
            ),
            StoreError::NotFound => {
                Self::new(StatusCode::NOT_FOUND, "not_found", "resource was not found")
            }
            StoreError::AlreadyExists => Self::new(
                StatusCode::CONFLICT,
                "application_name_collision",
                "application identity or name already exists",
            ),
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

/// Builds the TCP router, registering the dashboard when the binary embeds it.
pub fn router(state: ApiState) -> Router {
    web_router(state, UiAssets::resolve())
}

/// Builds the API-only router used by the Unix-socket client transport.
pub fn api_router(state: ApiState) -> Router {
    let (router, openapi) = documented_router().split_for_parts();
    finish_router(router.fallback(api_fallback), state, openapi)
}

/// Builds the TCP router from the API, liveness, and optional UI boundaries.
pub fn web_router(state: ApiState, ui_assets: UiAssets) -> Router {
    let (router, openapi) = documented_router().split_for_parts();
    let router = router.merge(health_router());
    let router = match ui_assets {
        UiAssets::Disabled => router.fallback(api_fallback),
        UiAssets::Embedded(bundle) => router
            .route("/", get(ui::redirect))
            .route("/dashboard", get(ui::redirect))
            .fallback(move |request: Request| ui_fallback(bundle, request)),
    };
    finish_router(router, state, openapi)
}

/// Builds the liveness-only route set. It is intentionally not part of Utoipa.
pub fn health_router() -> Router<ApiState> {
    Router::<ApiState>::new().route("/health", get(system::health))
}

fn finish_router(
    router: Router<ApiState>,
    state: ApiState,
    openapi: utoipa::openapi::OpenApi,
) -> Router {
    let request_id = header::HeaderName::from_static("x-request-id");
    // 405 responses must advertise exactly the methods each matched endpoint
    // registers, so the values are derived from the OpenAPI document itself.
    let allow_routes = AllowRoutes::build(&openapi);
    let router = router.method_not_allowed_fallback(move |request: Request| {
        let allow_routes = Arc::clone(&allow_routes);
        async move { method_not_allowed(&allow_routes, request.uri().path()) }
    });
    router
        .with_state(state)
        .layer(Extension(Arc::new(openapi)))
        .layer(middleware::from_fn(host_allowlist))
        // Both layers below wrap the allowlist: the propagator stamps even
        // short-circuited host rejections with their request ID, and the
        // binder echoes that same identifier in every structured error body.
        .layer(PropagateRequestIdLayer::new(request_id.clone()))
        .layer(middleware::from_fn(bind_error_request_id))
        .layer(SetRequestIdLayer::new(request_id, MakeRequestUuid))
        .layer(DefaultBodyLimit::max(REQUEST_BODY_LIMIT_BYTES))
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
    let lowered = raw.to_ascii_lowercase();
    // Bracketed IPv6 literals carry the port outside the brackets.
    let authority = if let Some(rest) = lowered.strip_prefix('[') {
        let Some((head, suffix)) = rest.split_once(']') else {
            return false;
        };
        if !suffix.is_empty()
            && !suffix.strip_prefix(':').is_some_and(|port| {
                !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            return false;
        }
        head
    } else {
        match lowered.rsplit_once(':') {
            // A digits-only suffix is a port unless the head itself contains
            // colons, which means this is an unbracketed IPv6 literal such as
            // `::1`.
            Some((head, tail))
                if !tail.is_empty()
                    && tail.bytes().all(|byte| byte.is_ascii_digit())
                    && !head.contains(':') =>
            {
                head
            }
            _ => lowered.as_str(),
        }
    };
    authority == "localhost"
        || authority
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

// Public endpoints must be registered through `routes!` here so Axum and the
// generated OpenAPI document receive the same method and path at the same time.
fn documented_router() -> OpenApiRouter<ApiState> {
    OpenApiRouter::with_openapi(openapi::base_document())
        .routes(routes!(system::status))
        .routes(routes!(openapi::openapi))
        .routes(routes!(applications::list))
        .routes(routes!(applications::apply))
        .routes(routes!(applications::plan))
        .routes(routes!(applications::get, applications::delete))
        .routes(routes!(applications::detail))
        .routes(routes!(applications::status))
        .routes(routes!(applications::reconcile))
        .routes(routes!(applications::refresh))
        .routes(routes!(events::list))
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

async fn ui_fallback(bundle: &'static EmbeddedBundle, request: Request) -> Response {
    if ui::is_api_path(request.uri().path()) {
        return api_fallback(request).await;
    }
    if request.uri().path().starts_with("/dashboard/") {
        return ui::serve(bundle, &request);
    }
    ui::not_found()
}

async fn api_fallback(request: Request) -> Response {
    if ui::is_api_path(request.uri().path()) {
        return ApiError::new(
            StatusCode::NOT_FOUND,
            "endpoint_not_found",
            "API endpoint was not found",
        )
        .into_response();
    }
    ui::not_found()
}
fn method_not_allowed(allow_routes: &AllowRoutes, path: &str) -> ApiError {
    let mut error = ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "HTTP method is not allowed",
    );
    // The header advertises only methods registered for the matched route;
    // when no documented route matches, the header is omitted.
    error.allow = if path == "/health" {
        Some("GET, HEAD".into())
    } else {
        allow_routes.allow_for(path)
    };
    error
}

/// Per-route `Allow` values derived from the `OpenAPI` document.
#[derive(Clone)]
struct AllowRoutes(Arc<[(Vec<String>, String)]>);

impl AllowRoutes {
    fn build(document: &utoipa::openapi::OpenApi) -> Arc<Self> {
        let mut routes = Vec::new();
        for (path, item) in &document.paths.paths {
            let methods = Self::path_methods(item);
            if !methods.is_empty() {
                routes.push((
                    path.split('/')
                        .filter(|segment| !segment.is_empty())
                        .map(str::to_owned)
                        .collect::<Vec<_>>(),
                    methods.join(", "),
                ));
            }
        }
        Arc::new(Self(routes.into()))
    }

    fn path_methods(item: &utoipa::openapi::path::PathItem) -> Vec<&'static str> {
        let mut methods = Vec::new();
        if item.get.is_some() {
            methods.push("GET");
            methods.push("HEAD");
        }
        if item.post.is_some() {
            methods.push("POST");
        }
        if item.put.is_some() {
            methods.push("PUT");
        }
        if item.patch.is_some() {
            methods.push("PATCH");
        }
        if item.delete.is_some() {
            methods.push("DELETE");
        }
        if item.head.is_some() {
            methods.push("HEAD");
        }
        methods
    }

    /// Returns the comma-separated methods for the first route template that
    /// matches the concrete request path, or `None` when none does.
    fn allow_for(&self, request_path: &str) -> Option<String> {
        let segments: Vec<&str> = request_path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();
        self.0
            .iter()
            .find(|(template, _)| {
                template.len() == segments.len()
                    && template
                        .iter()
                        .zip(&segments)
                        .all(|(expected, actual)| expected.starts_with('{') || expected == actual)
            })
            .map(|(_, allow)| allow.clone())
    }
}

fn ok<T: Serialize>(data: T) -> impl IntoResponse {
    (StatusCode::OK, axum::Json(Envelope { data }))
}
fn accepted(data: AcceptedOperation) -> Response {
    (StatusCode::ACCEPTED, axum::Json(Envelope { data })).into_response()
}
fn content_type(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).trim())
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
        let path = piqueld_core::manifest::safe_decode_path(&error.path().to_string());
        tracing::debug!(%path, "request JSON was rejected");
        malformed()
    })?;
    deserializer.end().map_err(|_| malformed())?;
    Ok(value)
}

fn parse_manifest(
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(piqueld_core::ValidatedApplication, Option<u64>), ApiError> {
    match content_type(headers) {
        Some(value) if value.eq_ignore_ascii_case(JSON) => {
            let request: ApplyApplicationRequest = decode_json(body)?;
            Ok((request.manifest.validate()?, request.expected_generation))
        }
        Some(value)
            if value.eq_ignore_ascii_case(TOML) || value.eq_ignore_ascii_case("text/toml") =>
        {
            let text = std::str::from_utf8(body).map_err(|_| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "toml_malformed",
                    "request TOML is malformed",
                )
            })?;
            let expected = if headers.get_all("x-expected-generation").iter().count() > 1 {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "generation_invalid",
                    "expected generation must occur once",
                ));
            } else {
                headers
                    .get("x-expected-generation")
                    .map(|value| {
                        value
                            .to_str()
                            .ok()
                            .and_then(|value| value.parse::<u64>().ok())
                            .ok_or_else(|| {
                                ApiError::new(
                                    StatusCode::BAD_REQUEST,
                                    "generation_invalid",
                                    "expected generation must be an unsigned integer",
                                )
                            })
                    })
                    .transpose()?
            };
            Ok((piqueld_core::parse_toml(text)?, expected))
        }
        _ => Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content_type_unsupported",
            "Content-Type must be application/json or application/toml",
        )),
    }
}

impl From<ApplicationError> for ApiError {
    fn from(error: ApplicationError) -> Self {
        match error {
            ApplicationError::Store(error) => error.into(),
            ApplicationError::Runtime(error) => error.into(),
            ApplicationError::PlanBlocked(diagnostics) => Self::new(
                StatusCode::CONFLICT,
                "plan_blocked",
                "runtime plan contains blocking conflicts",
            )
            .details(json!({"diagnostics":diagnostics})),
        }
    }
}
