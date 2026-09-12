use super::{ApiError, ApiState, ok, openapi::ApiErrorResponse};
use axum::{
    extract::{Query, State, rejection::QueryRejection},
    http::StatusCode,
    response::IntoResponse,
};
use piqueld_core::{
    ApplicationId, Event,
    api::{Envelope, Page},
};
use serde::Deserialize;

#[derive(Default, Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in=Query)]
pub(super) struct EventQuery {
    application_id: Option<String>,
    cursor: Option<String>,
    #[param(minimum = 1, maximum = 100)]
    limit: Option<usize>,
}

#[utoipa::path(get,path="/api/v1/events",operation_id="listEvents",params(EventQuery),
    responses((status=200,description="Informational history, oldest first",body=Envelope<Page<Event>>),
    (status=400,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn list(
    State(state): State<ApiState>,
    query: Result<Query<EventQuery>, QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Query(query) = query.map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "pagination_invalid",
            "invalid event query",
        )
    })?;
    let id = query.application_id.map(ApplicationId::parse).transpose()?;
    Ok(ok(state
        .store
        .events(
            id.as_ref(),
            query.cursor.as_deref(),
            query.limit.unwrap_or(50),
        )
        .await?))
}
