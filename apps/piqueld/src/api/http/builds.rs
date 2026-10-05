use super::{ApiError, ApiPath, ApiState, ok, openapi::ApiErrorResponse};
use crate::auth::Identity;
use axum::{
    Extension,
    extract::{Query, State, rejection::QueryRejection},
    http::StatusCode,
    response::IntoResponse,
};
use piqueld_core::{
    ApplicationId, EnvironmentId,
    access::AppPermission,
    api::{BuildLogPage, BuildRecord, Envelope, Page},
};
use serde::Deserialize;

#[derive(Default, Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in=Query)]
pub(super) struct BuildQuery {
    /// Only include builds of this application's environments.
    application_id: Option<String>,
    /// Only include builds for this environment.
    environment_id: Option<String>,
    /// `next_cursor` from a previous page.
    cursor: Option<String>,
    /// Page size; defaults to 50.
    #[param(minimum = 1, maximum = 100)]
    limit: Option<usize>,
}

/// Lists Git source builds, newest first.
///
/// Optionally filtered to one application or environment. Follow `next_cursor` to load older
/// builds.
#[utoipa::path(get,path="/api/v1/builds",operation_id="listBuilds",params(BuildQuery),
    responses((status=200,description="Build history, newest first",body=Envelope<Page<BuildRecord>>),
    (status=400,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn list(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    query: Result<Query<BuildQuery>, QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Query(query) = query.map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "pagination_invalid",
            "invalid build query",
        )
    })?;
    let application = query.application_id.map(ApplicationId::parse).transpose()?;
    let environment = query.environment_id.map(EnvironmentId::parse).transpose()?;
    Ok(ok(state
        .builds(
            application.as_ref(),
            environment.as_ref(),
            &identity.grants.app_scope(AppPermission::Read),
            query.cursor.as_deref(),
            query.limit.unwrap_or(50),
        )
        .await?))
}

#[derive(Default, Deserialize, utoipa::IntoParams)]
#[serde(default, deny_unknown_fields)]
#[into_params(parameter_in=Query)]
pub(super) struct OutputQuery {
    /// `previous_offset` from a previous page, to load older output.
    before: Option<i64>,
    /// Only include this output stream.
    stream: Option<piqueld_core::api::LogStream>,
}
/// Gets captured output for one build.
///
/// Returns the newest bounded page of output in chronological order. Pass the
/// page's `previous_offset` as `before` to load older output.
#[utoipa::path(get,path="/api/v1/builds/{id}/logs",operation_id="buildLogs",params(("id"=i64,Path),OutputQuery), responses((status=200,description="Bounded build output",body=Envelope<BuildLogPage>),(status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn logs(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiPath(id): ApiPath<i64>,
    query: Result<Query<OutputQuery>, QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Query(query) = query.map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "build_query_invalid",
            "invalid build log query",
        )
    })?;
    identity
        .grants
        .require_app(AppPermission::LogsRead, &state.build_application(id).await?)?;
    Ok(ok(state.build_logs(id, query.before, query.stream).await?))
}
