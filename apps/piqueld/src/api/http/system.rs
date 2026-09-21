use axum::{extract::State, http::StatusCode, response::IntoResponse};
use piqueld_core::api::{Envelope, SystemStatus};

use super::{ApiState, ok};

#[utoipa::path(
    get,
    path = "/api/v1/system/status",
    operation_id = "systemStatus",
    summary = "Get daemon status",
    responses(
        (status = 200, description = "Success", body = Envelope<SystemStatus>),
    )
)]
pub(super) async fn status(State(state): State<ApiState>) -> impl IntoResponse {
    ok(state.system_status())
}

#[derive(serde::Serialize)]
struct HealthResponse {
    status: &'static str,
}

pub(super) async fn health() -> impl IntoResponse {
    (StatusCode::OK, axum::Json(HealthResponse { status: "ok" }))
}

#[utoipa::path(get,path="/api/v1/system/configuration",operation_id="systemConfiguration",
    responses((status=200,description="Effective read-only host settings",body=Envelope<piqueld_core::api::HostConfiguration>),
    (status=503,response=inline(super::openapi::ApiErrorResponse))))]
pub(super) async fn configuration(
    State(state): State<ApiState>,
) -> Result<impl IntoResponse, super::ApiError> {
    Ok(ok(state.configuration()?.clone()))
}

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
