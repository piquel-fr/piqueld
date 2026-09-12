use super::{ApiError, ApiState, ok, openapi::ApiErrorResponse};
use axum::{
    extract::{Path, State},
    response::IntoResponse,
};
use piqueld_core::Operation;
use piqueld_core::api::Envelope;

#[utoipa::path(
    get,
    path = "/api/v1/operations/{id}",
    operation_id = "getOperation",
    summary = "Get an operation",
    params(("id" = String, Path, min_length = 8, max_length = 128)),
    responses(
        (status = 200, description = "Success", body = Envelope<Operation>),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn get(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state.store.operation(&id).await?))
}
