use axum::{extract::State, http::StatusCode, response::IntoResponse};
use piqueld_core::api::{Envelope, SystemStatus};

use super::{ApiState, ok};

/// Gets daemon status.
///
/// Reports the daemon version, API version, instance ID, and the recency of the
/// last successful `piqueld backup`. Returns 503 when control-plane storage
/// cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/system/status",
    operation_id = "systemStatus",
    summary = "Get daemon status",
    responses(
        (status = 200, description = "Success", body = Envelope<SystemStatus>),
        (status = 503, response = inline(super::openapi::ApiErrorResponse)),
    )
)]
pub(super) async fn status(
    State(state): State<ApiState>,
) -> Result<impl IntoResponse, super::ApiError> {
    Ok(ok(state.system_status().await?))
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
    Ok(ok(state.configuration()?.clone()))
}

/// Checks whether deployment dependencies are ready.
///
/// Probes the database, Docker Engine, and the Swarm manager with bounded
/// timeouts, and reports ingress status. The same body is returned in both
/// cases; only the status code differs (200 when ready, 503 otherwise).
#[utoipa::path(get,path="/api/v1/system/readiness",operation_id="systemReadiness",
 responses((status=200,description="Deployment dependencies ready",body=Envelope<piqueld_core::api::ReadinessStatus>),
 (status=503,description="Deployment dependencies unavailable",body=Envelope<piqueld_core::api::ReadinessStatus>)))]
pub(super) async fn readiness(State(state): State<ApiState>) -> impl IntoResponse {
    let status = state.readiness().await;
    (
        if status.ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        axum::Json(Envelope { data: status }),
    )
}
