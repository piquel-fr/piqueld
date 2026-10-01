//! Versioned HTTP/JSON API boundary.

use axum::{
    Extension, Router,
    body::Body,
    extract::{DefaultBodyLimit, MatchedPath, RawPathParams, Request},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use piqueld_core::ApplicationIdError;
use piqueld_core::api::{ApplyApplicationRequest, Envelope, ErrorBody};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, RequestId, SetRequestIdLayer},
    trace::TraceLayer,
};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::store::StoreError;

mod applications;
mod auth;
pub use auth::Authenticator;
mod browser;
mod builds;
mod deployments;
mod editing;
mod events;
mod logs;
mod observability;
mod openapi;
mod operations;
mod secrets;
mod system;
mod ui;

use crate::api::ApplicationError;
pub use crate::api::ApplicationService as ApiState;
use crate::application::BoundaryError;
pub use openapi::openapi_document;
pub use ui::{EmbeddedBundle, UiAssets};

/// Media type for JSON request and response bodies.
const JSON: &str = "application/json";
/// Media type accepted for raw TOML manifest uploads.
const TOML: &str = "application/toml";

/// Upper bound for one API request body. The CLI's manifest preflight limit
/// (`piquelctl::support::MAX_MANIFEST_BYTES`) must not exceed this value, or a
/// locally accepted manifest would fail server-side with 413.
const REQUEST_BODY_LIMIT_BYTES: usize = 2 * 1024 * 1024;

/// Handler failure rendered as a structured JSON `ErrorBody` response.
///
/// Every domain error converts into this type, so handlers can use `?` and
/// still produce a stable machine-readable `code` plus a sanitized message.
#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    /// Stable machine-readable error code, e.g. `generation_conflict`.
    code: &'static str,
    /// Sanitized, user-facing explanation.
    message: &'static str,
    /// Extra structured context; `null` when there is none.
    details: Value,
    /// `Allow` header value, only set for 405 responses.
    allow: Option<String>,
    /// Diagnostic attached as a response extension so `bind_error_request_id`
    /// can record it instead of synthesizing a generic one.
    diagnostic: Option<Box<piqueld_core::observability::Diagnostic>>,
}

impl ApiError {
    /// Creates an error without details, `Allow` header, or diagnostic.
    fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
            details: Value::Null,
            allow: None,
            diagnostic: None,
        }
    }
    /// Attaches structured `details` to the error body.
    fn details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }

    /// Logs storage failures that indicate a server-side fault. Expected client
    /// errors (conflicts, not found, validation) are deliberately not logged.
    fn log_storage_error(error: &StoreError) {
        if matches!(
            error,
            StoreError::SecretSource(_)
                | StoreError::Database
                | StoreError::DatabaseSource(_)
                | StoreError::SchemaMismatch
                | StoreError::SchemaMismatchSource(_)
                | StoreError::PathSource(_)
                | StoreError::Corrupt
                | StoreError::CorruptSource(_)
        ) {
            tracing::error!(?error, "storage request failed");
        }
    }
}

impl ApiError {
    /// Maps secret lifecycle failures selected by `From<StoreError>`.
    ///
    /// Panics if given a non-secret variant; callers must pre-filter.
    fn from_secret_error(error: StoreError) -> Self {
        match error {
            StoreError::SecretVersionConflict { expected, actual } => Self::new(
                StatusCode::CONFLICT,
                "secret_generation_conflict",
                "Secret changed since inspection; read its metadata and retry",
            )
            .details(json!({"expected_generation": expected, "actual_generation": actual})),
            StoreError::SecretUnavailable { names } => Self::new(
                StatusCode::CONFLICT,
                "secret_unavailable",
                "Supply replacement secret values and start a new deployment",
            )
            .details(json!({"names": names})),
            StoreError::SecretKeyUsable => Self::new(
                StatusCode::CONFLICT,
                "secret_key_usable",
                "The secret master key still works; recovery would discard values needlessly",
            ),
            error @ StoreError::SecretSource(_) => {
                let diagnostic = crate::operations::OperationError::from(error).diagnostic();
                let mut error = Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "secret_storage_unavailable",
                    "Secret storage is unavailable",
                );
                error.diagnostic = Some(Box::new(diagnostic));
                error
            }
            StoreError::SecretDeleting => Self::new(
                StatusCode::CONFLICT,
                "secret_deleting",
                "Secret deletion is in progress; retry deletion to finish cleanup",
            ),
            StoreError::SecretQuota => Self::new(
                StatusCode::CONFLICT,
                "secret_quota_exceeded",
                "Secret storage quota exceeded (1000 versions or 100 MiB per application); delete unused secrets to free space",
            ),
            StoreError::SecretReferenced => Self::new(
                StatusCode::CONFLICT,
                "secret_referenced",
                "Secret is still referenced by application configuration or a deployment",
            ),
            other => unreachable!("non-secret storage error: {other}"),
        }
    }
}

impl From<StoreError> for ApiError {
    /// Classifies storage failures into HTTP status codes and stable error
    /// codes, logging server-side faults first.
    fn from(value: StoreError) -> Self {
        Self::log_storage_error(&value);
        match value {
            StoreError::Validation(errors) => errors.into(),
            StoreError::Edit(error) => error.into(),
            error @ (StoreError::SecretVersionConflict { .. }
            | StoreError::SecretUnavailable { .. }
            | StoreError::SecretKeyUsable
            | StoreError::SecretSource(_)
            | StoreError::SecretDeleting
            | StoreError::SecretQuota
            | StoreError::SecretReferenced) => Self::from_secret_error(error),
            StoreError::HostnameConflict { hostname } => Self::new(
                StatusCode::CONFLICT,
                "hostname_conflict",
                "Hostname is reserved by another application or this installation",
            )
            .details(json!({"hostname": hostname})),
            StoreError::GenerationConflict { expected, actual } => Self::new(
                StatusCode::CONFLICT,
                "generation_conflict",
                "Application changed since inspection; run the command again",
            )
            .details(
                serde_json::json!({"expected_generation":expected,"actual_generation":actual}),
            ),
            StoreError::IdentityConflict => Self::new(
                StatusCode::CONFLICT,
                "identity_conflict",
                "Application changed since inspection; run the command again",
            ),
            StoreError::ReplayConflict => Self::new(
                StatusCode::CONFLICT,
                "request_id_conflict",
                "request ID was already used for different input",
            ),
            StoreError::RepositoryManaged => Self::new(
                StatusCode::CONFLICT,
                "repository_managed",
                "Edit runtime configuration in the repository manifest; only its connection settings can be changed directly",
            ),
            StoreError::Busy => Self::new(
                StatusCode::CONFLICT,
                "application_busy",
                "application is busy; wait for its current operation to finish",
            ),
            StoreError::HistoryExpired => Self::new(
                StatusCode::GONE,
                "history_expired",
                "Requested event history was pruned; reload history before resuming",
            ),
            StoreError::NotFound => {
                Self::new(StatusCode::NOT_FOUND, "not_found", "resource was not found")
            }
            StoreError::Lockout(lockout) => {
                Self::new(StatusCode::CONFLICT, "account_lockout", lockout.message())
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

impl From<piqueld_core::edit::EditError> for ApiError {
    /// Maps field-edit failures, exposing the edit reason in `details.reason`.
    fn from(error: piqueld_core::edit::EditError) -> Self {
        use piqueld_core::edit::EditError;
        let (status, code) = match &error {
            EditError::NotFound { .. } => (StatusCode::NOT_FOUND, "field_resource_not_found"),
            EditError::AlreadyExists { .. } => (StatusCode::CONFLICT, "field_resource_exists"),
            EditError::Incompatible(_) => (StatusCode::CONFLICT, "field_incompatible"),
        };
        Self::new(status, code, "The field edit could not be applied")
            .details(json!({"reason": error.to_string()}))
    }
}

impl From<BoundaryError> for ApiError {
    /// Maps runtime, build, and compilation failures, always attaching the
    /// boundary diagnostic so it is recorded with the request.
    fn from(value: BoundaryError) -> Self {
        let diagnostic = value.diagnostic();
        // Storage conversion handles its own logging, including expected client errors.
        if !matches!(&value, BoundaryError::Store(_)) {
            tracing::error!(error = ?value, "runtime boundary request failed");
        }
        let mut error = match value {
            BoundaryError::Store(error) => error.into(),
            BoundaryError::Runtime(
                crate::docker::DockerError::Unavailable(_)
                | crate::docker::DockerError::UnavailableSource { .. },
            ) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "docker_unavailable",
                "Docker Engine is unavailable",
            ),
            BoundaryError::Runtime(_) => Self::new(
                StatusCode::BAD_GATEWAY,
                "runtime_request_failed",
                "runtime request failed",
            ),
            BoundaryError::GitBuild(_) => Self::new(
                StatusCode::BAD_GATEWAY,
                "git_build_failed",
                "Git source build failed",
            ),
            BoundaryError::Compilation(_) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "application_compilation_failed",
                "application compilation failed",
            ),
        };
        error.diagnostic = Some(Box::new(diagnostic));
        error
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
    /// Reports pure decode failures as `toml_malformed` (400) and semantic
    /// validation failures as `manifest_validation_failed` (422) with the
    /// individual errors in `details.errors`.
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
    /// Renders the JSON error body. The placeholder `request_id` is replaced
    /// with the real one by `bind_error_request_id`.
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
        if let Some(diagnostic) = self.diagnostic {
            response.extensions_mut().insert(*diagnostic);
        }
        if let Some(allow) = self.allow
            && let Ok(value) = header::HeaderValue::from_str(&allow)
        {
            response.headers_mut().insert(header::ALLOW, value);
        }
        response
    }
}

/// Builds the TCP router, registering the dashboard when the binary embeds it.
pub fn router(state: ApiState, auth: impl Authenticator) -> Router {
    web_router(state, UiAssets::resolve(), auth)
}

/// Builds the API-only router used by the Unix-socket client transport.
pub fn api_router(state: ApiState, auth: impl Authenticator) -> Router {
    let (router, openapi) = documented_router().split_for_parts();
    finish_router(router.fallback(api_fallback), state, &openapi, auth, None)
}

/// Builds the TCP router from the API, liveness, and optional UI boundaries,
/// trusting only localhost and literal IP hosts.
pub fn web_router(state: ApiState, ui_assets: UiAssets, auth: impl Authenticator) -> Router {
    web_router_with_hosts(state, ui_assets, auth, Vec::new())
}

/// Builds a TCP router allowing additional explicitly trusted DNS hostnames.
/// Literal IP addresses and localhost are always allowed.
pub fn web_router_with_hosts(
    state: ApiState,
    ui_assets: UiAssets,
    auth: impl Authenticator,
    allowed_hosts: Vec<String>,
) -> Router {
    let (router, openapi) = documented_router().split_for_parts();
    let router = router.merge(health_router());
    let router = match ui_assets {
        UiAssets::Disabled => router.fallback(api_fallback),
        UiAssets::Embedded(bundle) => router
            .route("/", get(ui::redirect))
            .route("/dashboard", get(ui::redirect))
            .fallback(move |request: Request| ui_fallback(bundle, request)),
    };
    let router = finish_router(
        router,
        state,
        &openapi,
        auth,
        Some(browser::BrowserPolicy::new(allowed_hosts)),
    );
    match ui_assets {
        UiAssets::Disabled => router,
        UiAssets::Embedded(_) => router.layer(middleware::from_fn(ui::security_headers)),
    }
}

/// Builds the liveness-only route set. It is intentionally not part of Utoipa.
pub fn health_router() -> Router<ApiState> {
    Router::<ApiState>::new().route("/health", get(system::health))
}

/// Wraps a route set with the layers shared by every transport.
///
/// Layers, innermost first: browser trust policy (TCP only, when
/// `browser_policy` is set), authentication guard, state and `OpenAPI` 3.0
/// document extension, request ID propagation, error request ID binding and
/// diagnostic recording, request ID generation, body size limit, and tracing.
fn finish_router(
    router: Router<ApiState>,
    state: ApiState,
    openapi: &utoipa::openapi::OpenApi,
    auth: impl Authenticator,
    browser_policy: Option<browser::BrowserPolicy>,
) -> Router {
    let request_id = header::HeaderName::from_static("x-request-id");
    // 405 responses must advertise exactly the methods each matched endpoint
    // registers, so the values are derived from the OpenAPI document itself.
    let allow_routes = AllowRoutes::build(openapi);
    let openapi = openapi::openapi_30_document(openapi);
    let router = router.method_not_allowed_fallback(move |matched: Option<MatchedPath>| {
        let allow_routes = Arc::clone(&allow_routes);
        async move { method_not_allowed(&allow_routes, matched.as_ref()) }
    });
    let router = if let Some(policy) = browser_policy {
        router.layer(middleware::from_fn(move |request, next| {
            policy.clone().enforce(request, next)
        }))
    } else {
        router
    };
    // Authentication may reject requests without reaching a handler. Keep it
    // inside the shared request tracing and error/diagnostic response layers.
    auth.guard(router)
        .with_state(state.clone())
        .layer(Extension(Arc::new(openapi)))
        // The propagator stamps errors with their request ID, and the binder
        // echoes that same identifier in every structured error body.
        .layer(PropagateRequestIdLayer::new(request_id.clone()))
        .layer(middleware::from_fn_with_state(state, bind_error_request_id))
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

// Public endpoints must be registered through `routes!` here so Axum and the
// generated OpenAPI document receive the same method and path at the same time.
fn documented_router() -> OpenApiRouter<ApiState> {
    OpenApiRouter::with_openapi(openapi::base_document())
        .merge(editing::router())
        .routes(routes!(auth::status))
        .routes(routes!(auth::me))
        .routes(routes!(auth::register_start))
        .routes(routes!(auth::register_finish))
        .routes(routes!(auth::login_start))
        .routes(routes!(auth::login_finish))
        .routes(routes!(auth::logout))
        .routes(routes!(auth::directory))
        .routes(routes!(auth::manage))
        .routes(routes!(auth::device_start))
        .routes(routes!(auth::device_poll))
        .routes(routes!(auth::device_inspect))
        .routes(routes!(auth::device_approve))
        .routes(routes!(system::status))
        .routes(routes!(system::readiness))
        .routes(routes!(system::configuration))
        .routes(routes!(openapi::openapi))
        .routes(routes!(applications::list))
        .routes(routes!(applications::apply))
        .routes(routes!(applications::plan))
        .routes(routes!(applications::get, applications::delete))
        .routes(routes!(applications::detail))
        .routes(routes!(applications::manifest_download))
        .routes(routes!(applications::status))
        .routes(routes!(applications::reconcile))
        .routes(routes!(applications::rename))
        .routes(routes!(deployments::deploy))
        .routes(routes!(deployments::list))
        .routes(routes!(deployments::attempts))
        .routes(routes!(events::list))
        .routes(routes!(events::stream))
        .routes(routes!(observability::diagnostic))
        .routes(routes!(observability::resources))
        .routes(routes!(observability::analytics))
        .routes(routes!(observability::deliveries))
        .routes(routes!(observability::retry_delivery))
        .routes(routes!(logs::get))
        .routes(routes!(builds::list))
        .routes(routes!(builds::logs))
        .routes(routes!(operations::get))
        .routes(routes!(secrets::recover_key))
        .routes(routes!(secrets::list))
        .routes(routes!(secrets::put, secrets::delete))
}

/// Middleware that runs the request inside a `request_context` span and
/// post-processes JSON error responses.
///
/// 1. Extracts the application ID from `/api/v1/applications/{id}` routes.
/// 2. Runs the inner handler.
/// 3. For 4xx/5xx JSON `ErrorBody` responses, rewrites `request_id` to the
///    `x-request-id` value.
/// 4. For 5xx responses (except `configuration_unavailable`), records a
///    diagnostic (the handler's, or a synthesized one) and exposes its ID as
///    `details.diagnostic_id`. Storage failures (`storage_unavailable`,
///    `schema_mismatch`) are only logged, since the store cannot persist them.
///
/// Non-error and non-JSON responses pass through untouched.
async fn bind_error_request_id(
    axum::extract::State(state): axum::extract::State<ApiState>,
    matched: Option<MatchedPath>,
    params: Result<RawPathParams, axum::extract::rejection::RawPathParamsRejection>,
    request: Request,
    next: Next,
) -> Response {
    let application = matched
        .filter(|path| path.as_str().starts_with("/api/v1/applications/{id}"))
        .and_then(|_| params.ok())
        .and_then(|params| {
            params
                .iter()
                .find(|(name, _)| *name == "id")
                .and_then(|(_, id)| piqueld_core::ApplicationId::parse(id).ok())
        });
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .and_then(|value| value.header_value().to_str().ok())
        .map(str::to_owned);
    let response = {
        use tracing::Instrument as _;
        next.run(request)
            .instrument(tracing::info_span!(
                "request_context",
                request_id = request_id.as_deref().unwrap_or("unknown")
            ))
            .await
    };
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
    if parts.status.is_server_error() && error.code != "configuration_unavailable" {
        let diagnostic = parts
            .extensions
            .get::<piqueld_core::observability::Diagnostic>()
            .cloned()
            .unwrap_or_else(|| {
                piqueld_core::observability::Diagnostic::from_recorded_code(
                    format!("diagnostic-{}", uuid::Uuid::now_v7().simple()),
                    &error.code,
                    error.message.clone(),
                )
            });
        // Storage failures are not written back to the failing store: retrying the
        // write would only queue behind the global writer during the outage.
        if !matches!(
            error.code.as_str(),
            "storage_unavailable" | "schema_mismatch"
        ) {
            state
                .record_diagnostic(&diagnostic, Some(&error.request_id), application.as_ref())
                .await;
        }
        tracing::error!(diagnostic_id=%diagnostic.id, request_id=%error.request_id, code=%diagnostic.code, "API request failed");
        if !error.details.is_object() {
            error.details = json!({});
        }
        error.details["diagnostic_id"] = json!(diagnostic.id);
    }
    let bytes = serde_json::to_vec(&error).unwrap_or_else(|_| b"{}".to_vec());
    Response::from_parts(parts, Body::from(bytes))
}

/// Fallback when the dashboard is embedded: API paths get JSON 404s,
/// `/dashboard/*` is served from the bundle, anything else is a plain 404.
async fn ui_fallback(bundle: &'static EmbeddedBundle, request: Request) -> Response {
    if ui::is_api_path(request.uri().path()) {
        return api_fallback(request).await;
    }
    if request.uri().path().starts_with("/dashboard/") {
        return ui::serve(bundle, &request);
    }
    ui::not_found()
}

/// Fallback for unmatched routes: JSON `endpoint_not_found` for API paths,
/// plain 404 otherwise.
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
/// Builds the 405 response for a route that exists but not for this method.
/// `/health` is special-cased because it is not in the `OpenAPI` document.
fn method_not_allowed(allow_routes: &AllowRoutes, matched: Option<&MatchedPath>) -> ApiError {
    let mut error = ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "HTTP method is not allowed",
    );
    // The header advertises only methods registered for the matched route;
    // when no documented route matches, the header is omitted.
    error.allow = if matched.is_some_and(|path| path.as_str() == "/health") {
        Some("GET, HEAD".into())
    } else {
        matched.and_then(|path| allow_routes.0.get(path.as_str()).cloned())
    };
    error
}

/// Per-route `Allow` values derived from the `OpenAPI` document.
#[derive(Clone)]
struct AllowRoutes(HashMap<String, String>);

impl AllowRoutes {
    /// Indexes the `Allow` value for every documented path template.
    ///
    /// ```text
    /// "/api/v1/applications/{id}" -> "GET, HEAD, DELETE"
    /// ```
    fn build(document: &utoipa::openapi::OpenApi) -> Arc<Self> {
        let mut routes = HashMap::new();
        for (path, item) in &document.paths.paths {
            let methods = Self::path_methods(item);
            if !methods.is_empty() {
                routes.insert(path.clone(), methods.join(", "));
            }
        }
        Arc::new(Self(routes))
    }

    /// Lists the methods one path item registers. `GET` implies `HEAD`
    /// because Axum answers `HEAD` for every `GET` route.
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
}

/// Wraps `data` in the `{"data": ...}` envelope with 200 OK.
fn ok<T: Serialize>(data: T) -> impl IntoResponse {
    (StatusCode::OK, axum::Json(Envelope { data }))
}
/// Wraps `data` in the `{"data": ...}` envelope with 202 Accepted.
fn accepted<T: Serialize>(data: T) -> Response {
    (StatusCode::ACCEPTED, axum::Json(Envelope { data })).into_response()
}
/// Returns the media type from `Content-Type`, without parameters.
///
/// ```text
/// "application/json; charset=utf-8" -> "application/json"
/// ```
fn content_type(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).trim())
}
/// Strictly decodes a JSON body, rejecting trailing data. The failing field
/// path is only logged at debug level (sanitized); clients get a generic
/// `json_malformed` error.
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

/// Decodes and validates a manifest upload in either supported encoding.
///
/// Returns the validated application plus optional `expected_generation` and
/// `expected_application_id` preconditions:
/// - JSON: an `ApplyApplicationRequest` carrying the preconditions as fields.
/// - TOML (`application/toml` or `text/toml`): the raw manifest, with
///   preconditions in the single-valued `x-expected-generation` and
///   `x-expected-application-id` headers.
///
/// Any other content type fails with 415.
fn parse_manifest(
    headers: &HeaderMap,
    body: &[u8],
) -> Result<
    (
        piqueld_core::ValidatedApplication,
        Option<u64>,
        Option<String>,
    ),
    ApiError,
> {
    match content_type(headers) {
        Some(value) if value.eq_ignore_ascii_case(JSON) => {
            let request: ApplyApplicationRequest = decode_json(body)?;
            Ok((
                request.manifest.validate()?,
                request.expected_generation,
                request.expected_application_id,
            ))
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
            Ok((
                piqueld_core::parse_toml(text)?,
                expected,
                optional_header(headers, "x-expected-application-id")?,
            ))
        }
        _ => Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content_type_unsupported",
            "Content-Type must be application/json or application/toml",
        )),
    }
}

impl From<ApplicationError> for ApiError {
    /// Maps application service failures, delegating storage and runtime
    /// errors to their dedicated conversions.
    fn from(error: ApplicationError) -> Self {
        match error {
            ApplicationError::PreconditionRequired => Self::new(
                StatusCode::BAD_REQUEST,
                "precondition_required",
                "Supply the inspected revision and application identity, or explicitly set force=true",
            ),
            ApplicationError::InvalidPagination => Self::new(
                StatusCode::BAD_REQUEST,
                "pagination_invalid",
                "pagination parameters are invalid",
            ),
            ApplicationError::InvalidLogQuery => Self::new(
                StatusCode::BAD_REQUEST,
                "logs_query_invalid",
                "Tail must be 1–1000 and time window 1–86400 seconds",
            ),
            ApplicationError::ConfigurationUnavailable => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "configuration_unavailable",
                "Effective host configuration is unavailable",
            ),
            ApplicationError::ManifestSerialization(error) => {
                tracing::error!(?error, "serialize saved manifest");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "manifest_serialization_failed",
                    "Could not render saved configuration",
                )
            }
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

/// Reads an optional single-valued header; repeated or non-ASCII values are
/// rejected with `header_invalid`.
fn optional_header(headers: &HeaderMap, name: &str) -> Result<Option<String>, ApiError> {
    if headers.get_all(name).iter().count() > 1 {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "header_invalid",
            "mutation headers must occur once",
        ));
    }
    headers
        .get(name)
        .map(|value| {
            value.to_str().map(str::to_owned).map_err(|_| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "header_invalid",
                    "invalid mutation header",
                )
            })
        })
        .transpose()
}

/// Builds an isolated metrics-only router; no administrative routes are installed.
pub fn metrics_router(state: ApiState) -> Router {
    Router::new()
        .route(
            "/metrics",
            get(
                |axum::extract::State(state): axum::extract::State<ApiState>| async move {
                    match state.prometheus_metrics().await {
                        Ok(body) => (
                            StatusCode::OK,
                            [(
                                header::CONTENT_TYPE,
                                "text/plain; version=0.0.4; charset=utf-8",
                            )],
                            body,
                        ),
                        Err(_) => (
                            StatusCode::SERVICE_UNAVAILABLE,
                            [(header::CONTENT_TYPE, "text/plain")],
                            "Metrics collection unavailable\n".into(),
                        ),
                    }
                },
            ),
        )
        .with_state(state)
}

/// Path extractor that keeps Axum path decoding failures inside the API's
/// structured error contract, rejecting with a 400 `path_invalid` error.
struct ApiPath<T>(T);

impl<S, T> axum::extract::FromRequestParts<S> for ApiPath<T>
where
    S: Send + Sync,
    T: serde::de::DeserializeOwned + Send,
{
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        axum::extract::Path::<T>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Path(value)| Self(value))
            .map_err(|_| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "path_invalid",
                    "invalid URL path parameter",
                )
            })
    }
}
