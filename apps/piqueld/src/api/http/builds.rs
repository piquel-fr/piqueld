use super::{ApiError, ApiState, ok, openapi::ApiErrorResponse};
use axum::{
    extract::{Path, Query, State, rejection::QueryRejection},
    http::StatusCode,
    response::IntoResponse,
};
use piqueld_core::{
    ApplicationId,
    api::{BuildLogPage, BuildRecord, Envelope, Page},
};
use serde::Deserialize;

#[derive(Default, Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in=Query)]
pub(super) struct BuildQuery {
    application_id: Option<String>,
    cursor: Option<String>,
    #[param(minimum = 1, maximum = 100)]
    limit: Option<usize>,
}

#[utoipa::path(get,path="/api/v1/builds",operation_id="listBuilds",params(BuildQuery),
    responses((status=200,description="Build history, newest first",body=Envelope<Page<BuildRecord>>),
    (status=400,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn list(
    State(state): State<ApiState>,
    query: Result<Query<BuildQuery>, QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Query(query) = query.map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "pagination_invalid",
            "invalid build query",
        )
    })?;
    let id = query.application_id.map(ApplicationId::parse).transpose()?;
    Ok(ok(state
        .builds(
            id.as_ref(),
            query.cursor.as_deref(),
            query.limit.unwrap_or(50),
        )
        .await?))
}

#[derive(Default, Deserialize, utoipa::IntoParams)]
#[serde(default, deny_unknown_fields)]
#[into_params(parameter_in=Query)]
pub(super) struct OutputQuery {
    before: Option<i64>,
    stream: Option<piqueld_core::api::LogStream>,
}
#[utoipa::path(get,path="/api/v1/builds/{id}/logs",operation_id="buildLogs",params(("id"=i64,Path),OutputQuery), responses((status=200,description="Bounded build output",body=Envelope<BuildLogPage>),(status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn logs(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
    query: Result<Query<OutputQuery>, QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Query(query) = query.map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "build_query_invalid",
            "invalid build log query",
        )
    })?;
    Ok(ok(state.build_logs(id, query.before, query.stream).await?))
}
