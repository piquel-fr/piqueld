//! Git source build history and captured build output.
use super::{ApiError, ApiPath, ApiQuery, ApiState, QueryParams, ok, openapi::ApiErrorResponse};
use crate::auth::Identity;
use axum::{Extension, extract::State, response::IntoResponse};
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
    #[param(minimum = 1, maximum = 100, default = 50)]
    limit: Option<usize>,
}

impl QueryParams for BuildQuery {
    const INVALID_CODE: &'static str = "pagination_invalid";
    const INVALID_MESSAGE: &'static str = "invalid build query";
}

/// Lists Git source builds and job runs, newest first.
///
/// Optionally filtered to one application or environment. Follow `next_cursor` to load older
/// records.
#[utoipa::path(get,path="/api/v1/builds",operation_id="listBuilds",params(BuildQuery),
    responses((status=200,description="Build history, newest first",body=Envelope<Page<BuildRecord>>),
    (status=400,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn list(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiQuery(query): ApiQuery<BuildQuery>,
) -> Result<impl IntoResponse, ApiError> {
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

impl QueryParams for OutputQuery {
    const INVALID_CODE: &'static str = "build_query_invalid";
    const INVALID_MESSAGE: &'static str = "invalid build log query";
}

/// Gets captured output for one build.
///
/// Returns the newest bounded page of output in chronological order. Pass the
/// page's `previous_offset` as `before` to load older output.
#[utoipa::path(get,path="/api/v1/builds/{id}/logs",operation_id="buildLogs",params(("id"=i64,Path),OutputQuery), responses((status=200,description="Bounded build output",body=Envelope<BuildLogPage>),(status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn logs(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<i64>,
    ApiQuery(query): ApiQuery<OutputQuery>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state.build_logs(id, query.before, query.stream).await?))
}
