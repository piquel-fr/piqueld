use axum::{
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use piqueld_client::{Envelope, HealthStatus, ReadinessStatus, SystemCapabilities, SystemStatus};
use std::fmt::Write as _;

use super::{ApiState, RuntimeReadiness, ok, openapi::ApiErrorResponse};

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

#[utoipa::path(
    get,
    path = "/health",
    operation_id = "health",
    summary = "Check daemon health",
    responses(
        (status = 200, description = "Daemon is serving requests", body = HealthStatus)
    )
)]
pub(super) async fn health() -> impl IntoResponse {
    (
        StatusCode::OK,
        axum::Json(HealthStatus {
            status: "ok".into(),
        }),
    )
}

#[utoipa::path(
    get,
    path = "/api/v1/system/health",
    operation_id = "systemHealth",
    summary = "Check process liveness",
    responses(
        (status = 200, description = "The process event loop is serving requests", body = Envelope<HealthStatus>),
        (status = 401, response = inline(ApiErrorResponse))
    )
)]
pub(super) async fn health_v1() -> impl IntoResponse {
    ok(HealthStatus {
        status: "alive".into(),
    })
}

#[utoipa::path(
    get,
    path = "/api/v1/system/readiness",
    operation_id = "systemReadiness",
    summary = "Check required dependencies",
    responses(
        (status = 200, description = "All required dependencies are ready", body = Envelope<ReadinessStatus>),
        (status = 401, response = inline(ApiErrorResponse)),
        (status = 503, description = "One or more dependencies are unavailable", body = Envelope<ReadinessStatus>)
    )
)]
pub(super) async fn readiness(State(state): State<ApiState>) -> impl IntoResponse {
    let database = tokio::time::timeout(std::time::Duration::from_secs(2), state.store.probe())
        .await
        .is_ok_and(|result| result.is_ok());
    let runtime =
        tokio::time::timeout(std::time::Duration::from_secs(5), state.runtime.readiness())
            .await
            .unwrap_or_else(|_| RuntimeReadiness {
                docker: false,
                swarm_manager: false,
                infrastructure: false,
                reason: Some("runtime dependencies are unavailable".into()),
            });
    let ready = database && runtime.docker && runtime.swarm_manager && runtime.infrastructure;
    let response = ReadinessStatus {
        ready,
        database,
        docker: runtime.docker,
        swarm_manager: runtime.swarm_manager,
        infrastructure: runtime.infrastructure,
        reason: (!database)
            .then_some("database is unavailable".into())
            .or(runtime.reason),
    };
    (
        if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        axum::Json(Envelope { data: response }),
    )
}

#[utoipa::path(
    get,
    path = "/api/v1/system/metrics",
    operation_id = "systemMetrics",
    summary = "Get optional low-cardinality metrics",
    responses(
        (status = 200, description = "Prometheus text metrics", content_type = "text/plain"),
        (status = 401, response = inline(ApiErrorResponse)),
        (status = 404, response = inline(ApiErrorResponse))
    )
)]
pub(super) async fn metrics(State(state): State<ApiState>) -> Response {
    if !state.metrics_enabled {
        return super::ApiError::new(StatusCode::NOT_FOUND, "not_found", "resource was not found")
            .into_response();
    }
    let database = tokio::time::timeout(std::time::Duration::from_secs(2), state.store.probe())
        .await
        .is_ok_and(|result| result.is_ok());
    let runtime = state.runtime.readiness().await;
    let ready = database && runtime.docker && runtime.swarm_manager && runtime.infrastructure;
    let mut body = format!(
        "# HELP piqueld_up Process event loop is alive.\n# TYPE piqueld_up gauge\npiqueld_up 1\n# HELP piqueld_ready Required control-plane dependencies are ready.\n# TYPE piqueld_ready gauge\npiqueld_ready {}\n# HELP piqueld_database_ready Control-plane database is available.\n# TYPE piqueld_database_ready gauge\npiqueld_database_ready {}\n# HELP piqueld_docker_ready Docker Engine is available.\n# TYPE piqueld_docker_ready gauge\npiqueld_docker_ready {}\n# HELP piqueld_swarm_manager_ready Docker is a single-node Swarm manager.\n# TYPE piqueld_swarm_manager_ready gauge\npiqueld_swarm_manager_ready {}\n# HELP piqueld_infrastructure_ready Registry and ingress infrastructure are ready.\n# TYPE piqueld_infrastructure_ready gauge\npiqueld_infrastructure_ready {}\n",
        u8::from(ready),
        u8::from(database),
        u8::from(runtime.docker),
        u8::from(runtime.swarm_manager),
        u8::from(runtime.infrastructure),
    );
    let _ = writeln!(body, "# EOF");
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}
