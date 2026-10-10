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
use piqueld_core::api::{ApplyApplicationRequest, Envelope, ErrorBody};
use piqueld_core::auth::HostOperator;
use piqueld_core::{
    ApplicationIdError, EnvironmentIdError, EnvironmentNameError, GitBranchError, PreviewSlotError,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, RequestId, SetRequestIdLayer},
    trace::TraceLayer,
};
use utoipa_axum::router::OpenApiRouter;

use crate::store::StoreError;

#[macro_use]
mod access;
mod applications;
mod auth;
pub use auth::Authenticator;
mod browser;
mod builds;
mod deployments;
mod editing;
mod environments;
mod events;
mod exec;
mod logs;
mod observability;
mod openapi;
mod operations;
mod previews;
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
    message: String,
    /// Extra structured context; `null` when there is none.
    details: Value,
    /// `Allow` header value, only set for 405 responses.
    allow: Option<String>,
    /// Diagnostic attached as a response extension so `bind_error_request_id`
    /// can record it instead of synthesizing a generic one.
    diagnostic: Option<Box<piqueld_core::observability::Diagnostic>>,
    /// Authorization refusal attached as a response extension, so the audit
    /// trail records it even when it reads as a plain 404.
    denied: Option<piqueld_core::access::Denied>,
}

impl ApiError {
    /// Creates an error without details, `Allow` header, or diagnostic.
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details: Value::Null,
            allow: None,
            diagnostic: None,
            denied: None,
        }
    }
    fn endpoint_not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "endpoint_not_found",
            "API endpoint was not found",
        )
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
            ref error @ StoreError::SecretMissing { ref names } => {
                Self::new(StatusCode::CONFLICT, "secret_missing", error.to_string())
                    .details(json!({"names": names}))
            }
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
                "Secret storage quota exceeded (1000 versions or 100 MiB per environment or application store); delete unused secrets to free space",
            ),
            StoreError::SecretReferenced => Self::new(
                StatusCode::CONFLICT,
                "secret_referenced",
                "Secret is still referenced by application configuration or a deployment",
            ),
            ref error @ StoreError::SecretAccessDenied {
                ref environment,
                ref secret,
            } => Self::new(
                StatusCode::CONFLICT,
                "secret_access_denied",
                error.to_string(),
            )
            .details(json!({"environment": environment, "secret": secret})),
            other => unreachable!("non-secret storage error: {other}"),
        }
    }

    /// Maps environment selection and rename failures selected by
    /// `From<StoreError>`, naming the environments in `details`, previews
    /// of applications without a manifest repository, and previews over a
    /// `[previews]` limit, listing the previews it counts in `details`.
    ///
    /// Panics if given another variant; callers must pre-filter.
    fn from_environment_error(error: StoreError) -> Self {
        match error {
            StoreError::EnvironmentRequired { environments } => Self::new(
                StatusCode::CONFLICT,
                "environment_required",
                "The application does not have exactly one environment; name the environment explicitly",
            )
            .details(json!({"environments": environments})),
            StoreError::ConfirmationRequired { environments } => Self::new(
                StatusCode::CONFLICT,
                "environment_confirmation_required",
                "Deleting this application also deletes all its environments; confirm by naming every one",
            )
            .details(json!({"environments": environments})),
            ref error @ StoreError::EnvironmentConfigured { ref environment } => Self::new(
                StatusCode::CONFLICT,
                "environment_configured",
                error.to_string(),
            )
            .details(json!({"environment": environment})),
            ref error @ StoreError::PreviewRequiresRepository => Self::new(
                StatusCode::CONFLICT,
                piqueld_core::codes::PREVIEW_REQUIRES_REPOSITORY,
                error.to_string(),
            ),
            ref error @ StoreError::PreviewLimitReached(ref reached) => Self::new(
                StatusCode::CONFLICT,
                piqueld_core::codes::PREVIEW_LIMIT_REACHED,
                error.to_string(),
            )
            .details(json!(reached)),
            other => unreachable!("non-environment storage error: {other}"),
        }
    }

    /// Maps hostname reservation conflicts selected by `From<StoreError>`,
    /// naming the hostname, and the sibling environment reserving it if any.
    ///
    /// Panics if given another variant; callers must pre-filter.
    fn from_hostname_conflict(error: StoreError) -> Self {
        match error {
            StoreError::HostnameConflict { hostname } => Self::new(
                StatusCode::CONFLICT,
                "hostname_conflict",
                "Hostname is reserved by another environment or this installation",
            )
            .details(json!({"hostname": hostname})),
            ref error @ StoreError::SharedHostnameConflict {
                ref hostname,
                ref environment,
            } => Self::new(StatusCode::CONFLICT, "hostname_conflict", error.to_string())
                .details(json!({"hostname": hostname, "environment": environment})),
            other => unreachable!("non-hostname storage error: {other}"),
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
            | StoreError::SecretMissing { .. }
            | StoreError::SecretKeyUsable
            | StoreError::SecretSource(_)
            | StoreError::SecretDeleting
            | StoreError::SecretQuota
            | StoreError::SecretReferenced
            | StoreError::SecretAccessDenied { .. }) => Self::from_secret_error(error),
            error @ (StoreError::HostnameConflict { .. }
            | StoreError::SharedHostnameConflict { .. }) => Self::from_hostname_conflict(error),
            error @ (StoreError::EnvironmentRequired { .. }
            | StoreError::ConfirmationRequired { .. }
            | StoreError::EnvironmentConfigured { .. }
            | StoreError::PreviewRequiresRepository
            | StoreError::PreviewLimitReached(_)) => Self::from_environment_error(error),
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
            StoreError::Denied(denied) => denied.into(),
            StoreError::CredentialRevoked => crate::auth::AuthError::Unauthorized.into(),
            StoreError::AlreadyExists => Self::new(
                StatusCode::CONFLICT,
                "application_name_collision",
                "application, environment, or preview identity or name already exists",
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

impl From<EnvironmentIdError> for ApiError {
    fn from(_: EnvironmentIdError) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "environment_id_invalid",
            "environment ID is invalid",
        )
    }
}

impl From<EnvironmentNameError> for ApiError {
    fn from(_: EnvironmentNameError) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "environment_name_invalid",
            "environment names must be 1-63 lowercase letters, digits, or hyphens, start with a letter, and end with a letter or digit",
        )
    }
}

impl From<GitBranchError> for ApiError {
    fn from(error: GitBranchError) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "git_branch_invalid",
            error.to_string(),
        )
    }
}

impl From<PreviewSlotError> for ApiError {
    fn from(error: PreviewSlotError) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "preview_slot_invalid",
            error.to_string(),
        )
    }
}

impl From<piqueld_core::ValidationErrors> for ApiError {
    /// Reports pure decode failures as `toml_malformed` (400) and semantic
    /// validation failures as `manifest_validation_failed` (422). Both list
    /// the individual errors in `details.errors`.
    fn from(piqueld_core::ValidationErrors(errors): piqueld_core::ValidationErrors) -> Self {
        let error = if errors
            .iter()
            .all(|error| error.code == piqueld_core::codes::MANIFEST_DECODE_FAILED)
        {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "toml_malformed",
                "request TOML is malformed or does not match the application schema",
            )
        } else {
            Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "manifest_validation_failed",
                "application manifest failed validation",
            )
        };
        error.details(json!({"errors": errors}))
    }
}

impl ApiError {
    /// Builds the public JSON error body. Its placeholder `request_id` is
    /// replaced with the real one by `bind_error_request_id`, or by the exec
    /// stream before it writes a terminal failure frame.
    fn body(&self) -> ErrorBody {
        ErrorBody {
            code: self.code.into(),
            message: self.message.clone(),
            details: self.details.clone(),
            request_id: uuid::Uuid::now_v7().simple().to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    /// Renders the JSON error body with the error's status, diagnostic
    /// extension, and optional `Allow` header.
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            [(header::CONTENT_TYPE, JSON)],
            axum::Json(self.body()),
        )
            .into_response();
        if let Some(diagnostic) = self.diagnostic {
            response.extensions_mut().insert(*diagnostic);
        }
        if let Some(denied) = self.denied {
            response.extensions_mut().insert(denied);
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

/// Marks requests that arrived over the Unix socket, whose group-restricted
/// access is trusted to retrieve the first-account setup link.
#[derive(Clone, Copy)]
struct UnixSocket;

/// The process on the other end of a Unix socket connection, read with
/// `SO_PEERCRED`. Serve the Unix socket with
/// `into_make_service_with_connect_info::<UnixPeer>()` to provide it.
#[derive(Clone, Copy, Debug)]
pub struct UnixPeer {
    /// Effective user ID of the connecting process, if the kernel reported it.
    pub uid: Option<u32>,
}

impl UnixPeer {
    /// The host operator behind a request: a Unix socket peer that is root
    /// or the daemon's own user. Members of the socket's group are not.
    fn host_operator(extensions: &axum::http::Extensions) -> Option<HostOperator> {
        extensions.get::<UnixSocket>()?;
        let axum::extract::ConnectInfo(peer) =
            extensions.get::<axum::extract::ConnectInfo<Self>>()?;
        let daemon = rustix::process::geteuid().as_raw();
        peer.uid
            .filter(|uid| *uid == 0 || *uid == daemon)
            .map(|uid| HostOperator { uid })
    }
}

impl
    axum::extract::connect_info::Connected<
        axum::serve::IncomingStream<'_, tokio::net::UnixListener>,
    > for UnixPeer
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, tokio::net::UnixListener>) -> Self {
        Self {
            uid: stream.io().peer_cred().ok().map(|peer| peer.uid()),
        }
    }
}

/// Builds the API-only router used by the Unix-socket client transport.
pub fn api_router(state: ApiState, auth: impl Authenticator) -> Router {
    let (router, openapi) = documented_router().split_for_parts();
    finish_router(router.fallback(api_fallback), state, &openapi, auth, None)
        .layer(Extension(UnixSocket))
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
/// `browser_policy` is set), route authorization, authentication guard, audit
/// trail, state and `OpenAPI` 3.0
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
    // registers, so the values are derived from the OpenAPI document itself,
    // as is every route's access requirement.
    let allow_routes = AllowRoutes::build(openapi);
    let route_access = access::RouteAccess::build(openapi);
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
    // Authorization runs after authentication, which may reject requests
    // without reaching a handler; auditing wraps both to record either's
    // refusals. Keep all three inside the shared request tracing and
    // error/diagnostic response layers.
    let router = router.layer(middleware::from_fn_with_state(
        (Arc::clone(&route_access), state.clone()),
        access::enforce,
    ));
    auth.guard(router)
        .layer(middleware::from_fn_with_state(
            (route_access, state.clone()),
            access::audit,
        ))
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
                // Successful requests (dashboard assets, polling) are routine,
                // so only failures are logged at the default level.
                .on_response(
                    |response: &Response, latency: std::time::Duration, _: &tracing::Span| {
                        let (status, latency_ms) = (
                            response.status(),
                            u64::try_from(latency.as_millis()).unwrap_or(u64::MAX),
                        );
                        if status.is_client_error() || status.is_server_error() {
                            tracing::info!(%status, latency_ms, "request completed");
                        } else {
                            tracing::debug!(%status, latency_ms, "request completed");
                        }
                    },
                ),
        )
}

// Endpoints must be registered here through `public!`, `authenticated!`, or
// `granted!` (see `access`), so Axum, the generated OpenAPI document, and the
// access table receive the same method, path, and requirement together.
fn documented_router() -> OpenApiRouter<ApiState> {
    OpenApiRouter::with_openapi(openapi::base_document())
        .merge(editing::router())
        .routes(public!(auth::status))
        .routes(public!(auth::setup_link))
        .routes(public!(auth::recover_admin))
        .routes(public!(auth::sign_in_link))
        .routes(authenticated!(auth::me))
        .routes(public!(auth::register_start))
        .routes(public!(auth::register_finish))
        .routes(public!(auth::login_start))
        .routes(public!(auth::login_finish))
        .routes(public!(auth::operator_sign_in))
        .routes(authenticated!(auth::logout))
        .routes(authenticated!(auth::directory))
        .routes(authenticated!(auth::manage))
        .routes(auth::device_start_route(public!(auth::device_start)))
        .routes(public!(auth::device_poll))
        .routes(authenticated!(auth::device_inspect))
        .routes(authenticated!(auth::device_approve))
        .routes(authenticated!(system::status))
        .routes(authenticated!(system::readiness))
        .routes(granted!(Global(SystemRead) => system::configuration))
        .routes(granted!(Global(SystemOperate) => system::refresh_dns))
        .routes(authenticated!(openapi::openapi))
        .routes(authenticated!(applications::list))
        .routes(authenticated!(applications::apply))
        .routes(authenticated!(applications::plan))
        .routes(granted!(App(Read) => applications::get))
        .routes(granted!(App(Delete) => applications::delete))
        .routes(granted!(App(Read) => applications::manifest_download))
        .routes(granted!(App(Write) => applications::rename))
        .routes(granted!(App(Write) => environments::create))
        .routes(granted!(App(Read) => environments::get))
        .routes(granted!(App(Delete) => environments::delete))
        .routes(granted!(App(Read) => environments::detail))
        .routes(granted!(App(Read) => environments::status))
        .routes(granted!(App(Deploy) => environments::reconcile))
        .routes(granted!(App(Write) => environments::rename))
        .routes(granted!(App(Write) => environments::branch))
        .routes(granted!(App(Deploy) => previews::create))
        .routes(granted!(App(Read) => previews::list))
        .routes(granted!(App(Read) => previews::get))
        .routes(granted!(App(Deploy) => previews::deploy))
        .routes(granted!(App(Delete) => previews::delete))
        .routes(granted!(App(Delete) => previews::prune))
        .routes(granted!(App(Deploy) => deployments::deploy))
        .routes(granted!(App(Read) => deployments::list))
        .routes(granted!(App(Read) => deployments::attempts))
        .routes(granted!(App(Read) => deployments::releases))
        .routes(authenticated!(events::list))
        .routes(authenticated!(events::stream))
        .routes(authenticated!(observability::diagnostic))
        .routes(granted!(Global(SystemRead) => observability::resources))
        .routes(granted!(App(EventsRead) => observability::analytics))
        .routes(granted!(Global(SystemRead) => observability::deliveries))
        .routes(granted!(Global(SystemOperate) => observability::retry_delivery))
        .routes(authenticated!(observability::audit))
        .routes(granted!(Global(AuditRead) => observability::verify_audit))
        .routes(granted!(App(LogsRead) => logs::get))
        .routes(granted!(App(Exec) => exec::exec))
        .routes(authenticated!(builds::list))
        .routes(granted!(App(LogsRead) => builds::logs))
        .routes(granted!(App(Read) => operations::get))
        .routes(granted!(Global(SystemOperate) => secrets::recover_key))
        .routes(granted!(App(Read) => secrets::list))
        .routes(granted!(App(SecretsWrite) => secrets::delete))
        .routes(granted!(App(SecretsWrite) => secrets::regenerate))
        .routes(granted!(App(Read) => secrets::list_stored))
        .routes(granted!(App(SecretsWrite) => secrets::put_stored, secrets::delete_stored))
        .routes(granted!(App(SecretsWrite) => secrets::set_access))
}

/// Middleware that runs the request inside a `request_context` span and
/// post-processes JSON error responses.
///
/// 1. Extracts the environment ID from `/api/v1/environments/{id}` and
///    `/api/v1/previews/{id}` routes.
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
    let environment = matched
        .filter(|path| {
            ["/api/v1/environments/{id}", "/api/v1/previews/{id}"]
                .iter()
                .any(|prefix| path.as_str().starts_with(prefix))
        })
        .and_then(|_| params.ok())
        .and_then(|params| {
            params
                .iter()
                .find(|(name, _)| *name == "id")
                .and_then(|(_, id)| piqueld_core::EnvironmentId::parse(id).ok())
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
    let diagnostic = parts
        .extensions
        .get::<piqueld_core::observability::Diagnostic>()
        .cloned();
    let identity = parts.extensions.get::<crate::auth::Identity>();
    let actor = identity.map_or(crate::store::Actor::Daemon, crate::auth::Identity::actor);
    state
        .record_failure(
            parts.status,
            &mut error,
            diagnostic,
            environment.as_ref(),
            actor.attribution(),
        )
        .await;
    let bytes = serde_json::to_vec(&error).unwrap_or_else(|_| b"{}".to_vec());
    Response::from_parts(parts, Body::from(bytes))
}

impl ApiState {
    /// Records a server error's diagnostic (`diagnostic`, or a synthesized one)
    /// in `environment`'s history, attributed to the request's `actor`, and
    /// exposes its ID as `details.diagnostic_id`. Client errors and
    /// `configuration_unavailable` are left untouched.
    async fn record_failure(
        &self,
        status: StatusCode,
        error: &mut ErrorBody,
        diagnostic: Option<piqueld_core::observability::Diagnostic>,
        environment: Option<&piqueld_core::EnvironmentId>,
        actor: crate::store::Attribution<'_>,
    ) {
        if !status.is_server_error() || error.code == "configuration_unavailable" {
            return;
        }
        let diagnostic = diagnostic.unwrap_or_else(|| {
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
            self.record_diagnostic(&diagnostic, Some(&error.request_id), environment, actor)
                .await;
        }
        tracing::error!(diagnostic_id=%diagnostic.id, request_id=%error.request_id, code=%diagnostic.code, "API request failed");
        if !error.details.is_object() {
            error.details = json!({});
        }
        error.details["diagnostic_id"] = json!(diagnostic.id);
    }
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
        return ApiError::endpoint_not_found().into_response();
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
        piqueld_core::manifest::ValidatedTemplate,
        Option<u64>,
        Option<String>,
    ),
    ApiError,
> {
    match content_type(headers) {
        Some(value) if value.eq_ignore_ascii_case(JSON) => {
            let request: ApplyApplicationRequest = decode_json(body)?;
            Ok((
                request.manifest.validate_template()?,
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
                piqueld_core::manifest::parse_template_toml(text)?,
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
            ApplicationError::ServiceNotRunning => Self::new(
                StatusCode::CONFLICT,
                "service_not_running",
                "Service has no running task; deploy it or check its health",
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
            ApplicationError::RepositoryUnavailable(error) => {
                tracing::warn!(?error, "manifest repository unavailable");
                Self::new(
                    StatusCode::BAD_GATEWAY,
                    "repository_unavailable",
                    format!(
                        "The manifest repository could not be read ({error:#}); no preview was deleted"
                    ),
                )
            }
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
