//! Versioned HTTP/JSON API boundary.

use axum::{
    Extension, Router,
    body::Body,
    extract::{DefaultBodyLimit, MatchedPath, RawPathParams, Request},
    http::{Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use piqueld_core::api::{Envelope, ErrorBody};
use piqueld_core::auth::HostOperator;
use serde::Serialize;
use serde_json::json;
use std::{collections::HashMap, sync::Arc};
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, RequestId, SetRequestIdLayer},
    trace::TraceLayer,
};
use utoipa_axum::router::OpenApiRouter;

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
mod error;
mod events;
mod exec;
mod extract;
mod lease;
mod logs;
mod observability;
mod openapi;
mod operations;
mod previews;
mod secrets;
mod system;
mod ui;
mod webhooks;
pub use webhooks::webhook_router;

pub use crate::api::ApplicationService as ApiState;
use error::ApiError;
use extract::{ApiJson, ApiPath, ApiQuery, QueryParams};
pub use openapi::openapi_document;
pub use ui::{EmbeddedBundle, UiAssets};

/// Media type for JSON request and response bodies.
const JSON: &str = "application/json";
/// Media type accepted for raw TOML manifest uploads.
const TOML: &str = "application/toml";

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

/// Wraps a [`web_router_with_hosts`] router for the dashboard socket, which
/// the gateway's private listener forwards `ingress.private.dashboard_hostname`
/// to. One upstream connection carries many tailnet clients' requests, so each
/// request's client address comes from the `X-Forwarded-For` header the
/// gateway sets. Only the daemon's user, which the gateway runs as, can
/// connect; a request without a single client address is refused.
pub fn gateway_router(router: Router) -> Router {
    router.layer(middleware::from_fn(
        |mut request: Request, next: Next| async move {
            let client = request
                .headers()
                .get("x-forwarded-for")
                .and_then(|value| value.to_str().ok()?.parse::<std::net::IpAddr>().ok());
            let Some(client) = client else {
                return ApiError::new(
                    StatusCode::FORBIDDEN,
                    "untrusted_client",
                    "request did not come through the gateway",
                )
                .into_response();
            };
            request
                .extensions_mut()
                .insert(axum::extract::ConnectInfo(std::net::SocketAddr::new(
                    client, 0,
                )));
            next.run(request).await
        },
    ))
}

/// Builds the liveness-only route set. It is intentionally not part of Utoipa.
pub fn health_router() -> Router<ApiState> {
    Router::<ApiState>::new().route("/health", get(system::health))
}

/// Wraps a route set with the layers shared by every transport.
///
/// Layers, innermost first: route authorization, authentication guard,
/// browser trust policy (TCP only, when `browser_policy` is set), audit
/// trail, state and `OpenAPI` 3.0
/// document extension, request ID propagation, error request ID binding and
/// diagnostic recording, request ID generation, body size limit, latency
/// metrics, and tracing.
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
    // Authorization runs after authentication, which may reject requests
    // without reaching a handler. The browser trust policy runs before both,
    // so untrusted requests are refused before they spend sign-in allowance
    // or tailnet lookups. Auditing wraps all three to record their refusals.
    // Keep them inside the shared request tracing and error/diagnostic
    // response layers.
    let router = router.layer(middleware::from_fn_with_state(
        (Arc::clone(&route_access), state.clone()),
        access::enforce,
    ));
    let router = auth.guard(router);
    let router = if let Some(policy) = browser_policy {
        router.layer(middleware::from_fn(move |request, next| {
            policy.clone().enforce(request, next)
        }))
    } else {
        router
    };
    router
        .layer(middleware::from_fn_with_state(
            (route_access, state.clone()),
            access::audit,
        ))
        .layer(middleware::from_fn(no_store))
        .with_state(state.clone())
        .layer(Extension(Arc::new(openapi)))
        // The propagator stamps errors with their request ID, and the binder
        // echoes that same identifier in every structured error body.
        .layer(PropagateRequestIdLayer::new(request_id.clone()))
        .layer(middleware::from_fn_with_state(state.clone(), bind_error_request_id))
        .layer(SetRequestIdLayer::new(request_id, MakeRequestUuid))
        .layer(DefaultBodyLimit::max(
            piqueld_core::api::MAX_REQUEST_BODY_BYTES,
        ))
        .layer(middleware::from_fn_with_state(state, record_latency))
        .layer(
            TraceLayer::new_for_http()
                // The `otel.*` and `http.*` fields shape the span telemetry
                // exports; see `crate::telemetry`.
                .make_span_with(|request: &Request| {
                    let method = request.method();
                    let route = request.extensions().get::<MatchedPath>().map(MatchedPath::as_str);
                    let span = tracing::info_span!(
                        "http_request",
                        method = %method,
                        path = %request.uri().path(),
                        otel.name = %route.map_or_else(|| method.to_string(), |route| format!("{method} {route}")),
                        otel.kind = "server",
                        otel.status_code = tracing::field::Empty,
                        http.request.method = %method,
                        http.route = tracing::field::Empty,
                        http.response.status_code = tracing::field::Empty,
                    );
                    if let Some(route) = route {
                        span.record("http.route", route);
                    }
                    span
                })
                // Successful requests (dashboard assets, polling) are routine,
                // so only failures are logged at the default level.
                .on_response(
                    |response: &Response, latency: std::time::Duration, span: &tracing::Span| {
                        let (status, latency_ms) = (
                            response.status(),
                            u64::try_from(latency.as_millis()).unwrap_or(u64::MAX),
                        );
                        span.record("http.response.status_code", status.as_u16());
                        if status.is_server_error() {
                            span.record("otel.status_code", "ERROR");
                        }
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
        .routes(granted!(App(Write) => environments::sync))
        .routes(granted!(Global(TelemetryGrant) => environments::telemetry))
        .routes(granted!(App(Read) => webhooks::get))
        .routes(granted!(App(Write) => webhooks::generate_secret))
        .routes(granted!(App(Write) => environments::source))
        .routes(granted!(App(Deploy) => environments::promote))
        .routes(granted!(App(Read) => environments::plan_promotion))
        .routes(granted!(App(PreviewsDeploy) => previews::create))
        .routes(granted!(App(Read) => previews::list))
        .routes(granted!(App(Read) => previews::get))
        .routes(granted!(App(PreviewsDeploy) => previews::deploy))
        .routes(granted!(App(PreviewsDeploy) => previews::deploy_local))
        .routes(granted!(App(PreviewsDelete) => previews::delete))
        .routes(granted!(App(PreviewsDelete) => previews::prune))
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

/// Middleware marking every API response, refusals included, `Cache-Control:
/// no-store`; other paths, like dashboard assets, keep their own caching.
async fn no_store(request: Request, next: Next) -> Response {
    let api = ui::is_api_path(request.uri().path());
    let mut response = next.run(request).await;
    if api {
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-store"),
        );
    }
    response
}

/// Middleware recording each request's latency, under its route template,
/// for telemetry export.
async fn record_latency(
    axum::extract::State(state): axum::extract::State<ApiState>,
    matched: Option<MatchedPath>,
    request: Request,
    next: Next,
) -> Response {
    let (method, started) = (request.method().clone(), std::time::Instant::now());
    let response = next.run(request).await;
    state.telemetry().record_request(
        &method,
        matched.as_ref().map(MatchedPath::as_str),
        response.status(),
        started.elapsed(),
    );
    response
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
    let environment = access::PathOwner::find(matched.as_ref(), params)
        .filter(|(owner, _)| *owner == access::PathOwner::Environment)
        .and_then(|(_, id)| piqueld_core::EnvironmentId::parse(id).ok());
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
                crate::operations::diagnostic_id(),
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
struct AllowRoutes(HashMap<String, String>);

impl AllowRoutes {
    /// Indexes the `Allow` value for every documented path template. `GET`
    /// implies `HEAD` because Axum answers `HEAD` for every `GET` route.
    ///
    /// ```text
    /// "/api/v1/applications/{id}" -> "GET, HEAD, DELETE"
    /// ```
    fn build(document: &utoipa::openapi::OpenApi) -> Arc<Self> {
        let routes = document
            .paths
            .paths
            .iter()
            .filter_map(|(path, item)| {
                let methods: Vec<_> = access::operations(item)
                    .flat_map(|(method, _)| {
                        let head = (method == Method::GET).then_some(Method::HEAD);
                        std::iter::once(method).chain(head)
                    })
                    .map(|method| method.to_string())
                    .collect();
                (!methods.is_empty()).then(|| (path.clone(), methods.join(", ")))
            })
            .collect();
        Arc::new(Self(routes))
    }
}

/// Wraps `data` in the `{"data": ...}` envelope with 200 OK.
fn ok<T: Serialize>(data: T) -> Response {
    (StatusCode::OK, axum::Json(Envelope { data })).into_response()
}

/// Wraps `data` in the `{"data": ...}` envelope with 202 Accepted.
fn accepted<T: Serialize>(data: T) -> Response {
    (StatusCode::ACCEPTED, axum::Json(Envelope { data })).into_response()
}
