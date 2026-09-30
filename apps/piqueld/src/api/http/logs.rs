use super::{ApiError, ApiPath, ApiState, ok, openapi::ApiErrorResponse};
use axum::{
    extract::{Query, State, rejection::QueryRejection},
    http::StatusCode,
    response::IntoResponse,
};
use piqueld_core::{
    ApplicationId,
    api::{ApplicationLogs, Envelope},
};
/// Query parameters for recent container output.
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[serde(default, deny_unknown_fields)]
#[into_params(parameter_in=Query)]
pub(super) struct LogQuery {
    /// Only include output from this service (1–63 characters); all services when omitted.
    service: Option<String>,
    /// Only include this output stream; merged output when omitted.
    stream: Option<piqueld_core::api::LogStream>,
    /// Maximum number of recent lines per service.
    #[param(minimum = 1, maximum = 1000, default = 200)]
    tail: u16,
    /// Only include output from this many seconds ago onward.
    #[param(minimum = 1, maximum = 86400, default = 3600)]
    since_seconds: u32,
}
impl Default for LogQuery {
    /// Matches the documented `tail` and `since_seconds` parameter defaults.
    fn default() -> Self {
        Self {
            service: None,
            stream: None,
            tail: 200,
            since_seconds: 3600,
        }
    }
}
/// Gets recent container output for an application.
///
/// Output can be narrowed to one service and one stream. Out-of-range `tail` or
/// `since_seconds` values fail with 400 `logs_query_invalid`.
#[utoipa::path(get,path="/api/v1/applications/{id}/logs",operation_id="applicationLogs",params(("id"=String,Path),LogQuery),
 responses((status=200,description="Recent Docker output",body=Envelope<ApplicationLogs>),
 (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),(status=502,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn get(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
    query: Result<Query<LogQuery>, QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Query(query) = query.map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "logs_query_invalid",
            "Invalid log query",
        )
    })?;
    let id = ApplicationId::parse(id)?;
    Ok(ok(state
        .logs(
            &id,
            query.service.as_deref(),
            query.tail,
            query.since_seconds,
            query.stream,
        )
        .await?))
}
