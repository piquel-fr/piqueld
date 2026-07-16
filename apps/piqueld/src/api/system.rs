use axum::{extract::State, http::header, response::IntoResponse};
use piqueld_client::{SystemCapabilities, SystemStatus};

use super::{ApiState, ok, openapi_document};

pub(super) async fn status(State(state): State<ApiState>) -> impl IntoResponse {
    ok(SystemStatus {
        status: "running".into(),
        api_version: "v1".into(),
        instance_id: state.instance_id,
    })
}

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

pub(super) async fn openapi() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/vnd.oai.openapi+json;version=3.1",
        )],
        openapi_document().to_string(),
    )
}
