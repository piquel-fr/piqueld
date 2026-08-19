use axum::{extract::State, http::StatusCode, response::IntoResponse};
use piqueld_client::{Envelope, SystemCapabilities, SystemStatus};

use super::{ApiState, ok, openapi::ApiErrorResponse};

#[utoipa::path(
    get,
    path = "/api/v1/system/status",
    operation_id = "systemStatus",
    summary = "Get daemon status",
    responses(
        (status = 200, description = "Success", body = Envelope<SystemStatus>),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn status(State(state): State<ApiState>) -> impl IntoResponse {
    ok(SystemStatus {
        status: "running".into(),
        api_version: "v1".into(),
        daemon_version: env!("CARGO_PKG_VERSION").into(),
        instance_id: state.instance_id,
    })
}

#[utoipa::path(
    get,
    path = "/api/v1/system/capabilities",
    operation_id = "systemCapabilities",
    summary = "Get daemon capabilities",
    responses(
        (status = 200, description = "Success", body = Envelope<SystemCapabilities>),
        (status = 500, response = inline(ApiErrorResponse))
    )
)]
pub(super) async fn capabilities(State(state): State<ApiState>) -> impl IntoResponse {
    ok(SystemCapabilities {
        persistence: true,
        source_resolution: true,
        runtime_observation: true,
        runtime_execution: true,
        secret_management: state.secret_service().is_some(),
        reason: None,
    })
}

#[derive(utoipa::ToSchema, serde::Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[utoipa::path(
    get,
    path = "/health",
    operation_id = "health",
    summary = "Check daemon health",
    responses(
        (status = 200, description = "Daemon is serving requests", body = HealthResponse)
    )
)]
pub(super) async fn health() -> impl IntoResponse {
    (StatusCode::OK, axum::Json(HealthResponse { status: "ok" }))
}
