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
    ok(SystemStatus {
        status: "running".into(),
        api_version: "v1".into(),
        daemon_version: env!("CARGO_PKG_VERSION").into(),
        instance_id: state.store.instance_id().to_owned(),
    })
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
    Ok(ok(state.configuration.ok_or_else(|| {
        super::ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "configuration_unavailable",
            "Effective host configuration is unavailable",
        )
    })?))
}

#[utoipa::path(get,path="/api/v1/system/readiness",operation_id="systemReadiness",
 responses((status=200,description="Deployment dependencies ready",body=Envelope<piqueld_core::api::ReadinessStatus>),
 (status=503,description="Deployment dependencies unavailable",body=Envelope<piqueld_core::api::ReadinessStatus>)))]
pub(super) async fn readiness(State(state): State<ApiState>) -> impl IntoResponse {
    use piqueld_core::api::{DependencyStatus, ReadinessStatus};
    let (database, runtime) = tokio::join!(
        tokio::time::timeout(std::time::Duration::from_secs(2), state.store.probe()),
        tokio::time::timeout(std::time::Duration::from_secs(5), state.runtime.readiness())
    );
    let database = database.is_ok_and(|r| r.is_ok());
    let (docker, swarm) = runtime.unwrap_or((false, false));
    let status = ReadinessStatus {
        ready: database && docker && swarm,
        database: DependencyStatus::new(database, "Database is unavailable or timed out"),
        docker: DependencyStatus::new(docker, "Docker Engine is unavailable or timed out"),
        swarm: DependencyStatus::new(swarm, "A compatible single-node Swarm manager is required"),
    };
    (
        if status.ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        axum::Json(Envelope { data: status }),
    )
}
