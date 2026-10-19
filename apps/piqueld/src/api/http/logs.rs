//! Recent container output of an environment's services.
use super::{ApiError, ApiPath, ApiQuery, ApiState, QueryParams, ok, openapi::ApiErrorResponse};
use axum::{extract::State, response::IntoResponse};
use piqueld_core::{
    EnvironmentId,
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
    /// Maximum number of most recent lines, merged across the selected services.
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

impl QueryParams for LogQuery {
    const INVALID_CODE: &'static str = "logs_query_invalid";
    const INVALID_MESSAGE: &'static str = "Invalid log query";
}

/// Gets recent container output for an environment.
///
/// Output can be narrowed to one service and one stream. Unknown or malformed
/// parameters, out-of-range `tail` or `since_seconds` values, and `service`
/// names outside 1–63 characters fail with 400 `logs_query_invalid`.
#[utoipa::path(get,path="/api/v1/environments/{id}/logs",operation_id="environmentLogs",params(("id"=String,Path),LogQuery),
 responses((status=200,description="Recent Docker output",body=Envelope<ApplicationLogs>),
 (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),(status=502,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn get(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
    ApiQuery(query): ApiQuery<LogQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let id = EnvironmentId::parse(id)?;
    let _admitted = state.admit(crate::api::Expensive::Logs)?;
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
