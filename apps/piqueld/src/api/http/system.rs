use crate::auth::Identity;
use axum::{
    Extension,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use piqueld_core::access::{AppPermission, GlobalPermission};
use piqueld_core::api::{BuilderStatus, DnsStatus, Envelope, SystemStatus};

use super::{ApiError, ApiState, ok};

/// Gets daemon status.
///
/// DNS providers and certificates are empty without `system:read`, and the
/// build machine is absent; see [`redact_host`]. Preview counts per
/// application list only readable applications.
#[utoipa::path(
    get,
    path = "/api/v1/system/status",
    operation_id = "systemStatus",
    summary = "Get daemon status",
    responses(
        (status = 200, description = "Success", body = Envelope<SystemStatus>),
    )
)]
pub(super) async fn status(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
) -> impl IntoResponse {
    let mut status = state
        .system_status(&identity.grants.app_scope(AppPermission::Read))
        .await;
    redact_host(&identity, &mut status);
    ok(status)
}

/// Empties host configuration in `status` without `system:read`: DNS
/// providers and certificates, which name every application's routed
/// hostnames, and the build machine.
fn redact_host(identity: &Identity, status: &mut SystemStatus) {
    if !identity.grants.has_global(GlobalPermission::SystemRead) {
        redact_dns(identity, &mut status.dns);
        status.builder = None;
    }
}

/// Empties `dns` without `system:read`; see [`redact_host`].
fn redact_dns(identity: &Identity, dns: &mut DnsStatus) {
    if !identity.grants.has_global(GlobalPermission::SystemRead) {
        *dns = DnsStatus::default();
    }
}

/// Checks the build machine now.
///
/// Connects to its `BuildKit` daemon, as before each build, and returns its
/// status with that check as its latest. 404 `builder_not_configured` when
/// the host builds images itself.
#[utoipa::path(
    post,
    path = "/api/v1/system/builder/check",
    operation_id = "checkBuilder",
    summary = "Check the build machine now",
    responses(
        (status = 200, description = "Success", body = Envelope<BuilderStatus>),
        (status = 404, response = inline(super::openapi::ApiErrorResponse)),
    )
)]
pub(super) async fn check_builder(State(state): State<ApiState>) -> Result<Response, ApiError> {
    let status = state.check_builder().await.ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "builder_not_configured",
            "this host has no build machine; it builds images itself",
        )
    })?;
    Ok(ok(status).into_response())
}

/// Checks DNS provider credentials now.
///
/// Lists every configured provider's zones immediately instead of at the next
/// hourly discovery, and returns the updated providers and certificates,
/// empty without `system:read` like [`status`]. Credential files are only
/// read at daemon startup.
#[utoipa::path(
    post,
    path = "/api/v1/system/dns/refresh",
    operation_id = "refreshDns",
    summary = "Check DNS providers now",
    responses(
        (status = 200, description = "Success", body = Envelope<DnsStatus>),
    )
)]
pub(super) async fn refresh_dns(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
) -> impl IntoResponse {
    let mut dns = state.refresh_dns().await;
    redact_dns(&identity, &mut dns);
    ok(dns)
}

/// Static liveness body: `{"status":"ok"}`.
#[derive(serde::Serialize)]
struct HealthResponse {
    status: &'static str,
}

/// Liveness probe. Always succeeds once the listener is serving and
/// deliberately touches no dependencies; see `readiness` for those.
pub(super) async fn health() -> impl IntoResponse {
    (StatusCode::OK, axum::Json(HealthResponse { status: "ok" }))
}

/// Gets the effective read-only host configuration.
///
/// Returns 503 when the daemon has no effective configuration to report.
#[utoipa::path(get,path="/api/v1/system/configuration",operation_id="systemConfiguration",
    responses((status=200,description="Effective read-only host settings",body=Envelope<piqueld_core::api::HostConfiguration>),
    (status=503,response=inline(super::openapi::ApiErrorResponse))))]
pub(super) async fn configuration(
    State(state): State<ApiState>,
) -> Result<impl IntoResponse, super::ApiError> {
    Ok(ok(state.host_configuration()?.clone()))
}

/// Checks whether deployment dependencies are ready.
///
/// Probes the database, Docker Engine, and the Swarm manager with bounded
/// timeouts, and reports ingress status. The same body is returned in both
/// cases; only the status code differs (200 when ready, 503 otherwise).
/// Ingress routes are listed only for applications the caller can read.
#[utoipa::path(get,path="/api/v1/system/readiness",operation_id="systemReadiness",
 responses((status=200,description="Deployment dependencies ready",body=Envelope<piqueld_core::api::ReadinessStatus>),
 (status=503,description="Deployment dependencies unavailable",body=Envelope<piqueld_core::api::ReadinessStatus>)))]
pub(super) async fn readiness(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
) -> impl IntoResponse {
    let status = state
        .readiness(&identity.grants.app_scope(AppPermission::Read))
        .await;
    (
        if status.ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        axum::Json(Envelope { data: status }),
    )
}

/// Checks the installation's health.
///
/// Inspects the live installation read-only: the public URL and its
/// certificate, Docker against ingress requirements, ports 80/443, the secret
/// key, the API socket's permissions, the tailnet node, and backups. Each
/// check passes, warns, fails, or is skipped, with a next step when action is
/// needed. Returns 503 when the daemon has no effective configuration, or
/// `server_busy` while two runs are already in progress.
#[utoipa::path(get,path="/api/v1/system/checks",operation_id="systemChecks",
    responses((status=200,description="Installation checks",body=Envelope<piqueld_core::api::SystemChecks>),
    (status=503,response=inline(super::openapi::ApiErrorResponse))))]
pub(super) async fn checks(
    State(state): State<ApiState>,
) -> Result<impl IntoResponse, super::ApiError> {
    let _admitted = state.admit(crate::api::Expensive::Checks)?;
    Ok(ok(state.system_checks().await?))
}
