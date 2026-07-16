use axum::{extract::State, response::IntoResponse};
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
        (status = 500, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn capabilities(State(state): State<ApiState>) -> impl IntoResponse {
    let capabilities = state.runtime.capabilities();
    ok(SystemCapabilities {
        persistence: true,
        source_resolution: capabilities.source_resolution,
        runtime_observation: capabilities.runtime_observation,
        runtime_execution: capabilities.runtime_execution,
        reason: capabilities.reason,
    })
}
