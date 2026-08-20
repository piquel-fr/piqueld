use super::{ApiError, ApiState, ok, openapi::ApiErrorResponse};
use axum::{
    extract::{Path, State},
    response::IntoResponse,
};
use piqueld_client::{Envelope, OperationStepView, OperationView};

/// Retrieves an operation and its associated steps by identifier.
///
/// # Arguments
///
/// * `id` - The operation identifier from the request path.
///
/// # Examples
///
/// ```
/// let id = "operation-123";
/// let path = format!("/api/v1/operations/{id}", id = id);
///
/// assert_eq!(path, "/api/v1/operations/operation-123");
/// ```
///
/// # Errors
///
/// Returns an `ApiError` if the operation cannot be loaded.
pub(super) async fn get(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let (operation, steps) = state.store.operation_with_steps(&id).await?;
    Ok(ok(OperationView {
        id: operation.id,
        application_id: operation.application_id.to_string(),
        generation: operation.generation,
        kind: operation.kind.as_str().into(),
        state: operation.state.as_str().into(),
        error_code: operation.error_code,
        error_message: operation.error_message,
        created_at_ms: operation.created_at_ms,
        updated_at_ms: operation.updated_at_ms,
        started_at_ms: operation.started_at_ms,
        finished_at_ms: operation.finished_at_ms,
        steps: steps
            .into_iter()
            .map(|step| OperationStepView {
                id: step.id,
                position: step.position,
                action: step.action,
                state: step.state.as_str().into(),
                attempt: step.attempt,
                error_code: step.error_code,
                error_message: step.error_message,
                updated_at_ms: step.updated_at_ms,
            })
            .collect(),
    }))
}
