use axum::{extract::State, http::StatusCode, response::IntoResponse};
use piqueld_client::{Envelope, SystemStatus};

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
    ok(SystemStatus {
        status: "running".into(),
        api_version: "v1".into(),
        daemon_version: env!("CARGO_PKG_VERSION").into(),
        instance_id: state.instance_id,
    })
}

#[derive(serde::Serialize)]
struct HealthResponse {
    status: &'static str,
}

pub(super) async fn health() -> impl IntoResponse {
    (StatusCode::OK, axum::Json(HealthResponse { status: "ok" }))
}
