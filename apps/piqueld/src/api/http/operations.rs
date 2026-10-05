use super::{ApiError, ApiPath, ApiState, ok, openapi::ApiErrorResponse};
use crate::auth::Identity;
use axum::{Extension, extract::State, response::IntoResponse};
use piqueld_core::Operation;
use piqueld_core::access::{AppPermission, Target};
use piqueld_core::api::Envelope;

// Returns one durable operation, as identified by a 202 mutation response.
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
    Extension(identity): Extension<Identity>,
    ApiPath(id): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    let operation = state.operation(&id).await?;
    let application = state
        .environment_application(&operation.environment_id)
        .await?;
    identity.grants.require_change(
        &[AppPermission::Read],
        application.as_ref().map_or(Target::Unknown, Target::Id),
    )?;
    Ok(ok(operation))
}
