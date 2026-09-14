use super::{ApiError, ApiState, ok, openapi::ApiErrorResponse};
use axum::{
    extract::{Path, Query, State, rejection::QueryRejection},
    http::StatusCode,
    response::IntoResponse,
};
use piqueld_core::{
    ApplicationId,
    api::{ApplicationLogs, Envelope},
};
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[serde(default, deny_unknown_fields)]
#[into_params(parameter_in=Query)]
pub(super) struct LogQuery {
    service: Option<String>,
    #[param(minimum = 1, maximum = 1000, default = 200)]
    tail: u16,
    #[param(minimum = 1, maximum = 86400, default = 3600)]
    since_seconds: u32,
}
impl Default for LogQuery {
    fn default() -> Self {
        Self {
            service: None,
            tail: 200,
            since_seconds: 3600,
        }
    }
}
#[utoipa::path(get,path="/api/v1/applications/{id}/logs",operation_id="applicationLogs",params(("id"=String,Path),LogQuery),
 responses((status=200,description="Recent Docker output",body=Envelope<ApplicationLogs>),
 (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),(status=502,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn get(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    query: Result<Query<LogQuery>, QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Query(query) = query.map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "logs_query_invalid",
            "Invalid log query",
        )
    })?;
    if !(1..=1000).contains(&query.tail)
        || !(1..=86400).contains(&query.since_seconds)
        || query
            .service
            .as_ref()
            .is_some_and(|s| s.is_empty() || s.len() > 63)
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "logs_query_invalid",
            "Tail must be 1–1000 and time window 1–86400 seconds",
        ));
    }
    let id = ApplicationId::parse(id)?;
    state.store.get(&id).await?;
    Ok(ok(state
        .runtime
        .logs(
            &id,
            query.service.as_deref(),
            query.tail,
            query.since_seconds,
        )
        .await?))
}
