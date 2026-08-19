use std::time::Duration;

use super::{
    ApiError, ApiState, StateEventSnapshot, current_state_stream, last_event_id, ok,
    openapi::ApiErrorResponse,
};
use crate::store::{Operation, OperationStep, WorkState};
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    response::{IntoResponse, Response, Sse, sse::KeepAlive},
};
use piqueld_client::{Envelope, OperationStepView, OperationView};

#[utoipa::path(
    get,
    path = "/api/v1/operations/{id}",
    operation_id = "getOperation",
    summary = "Get an operation",
    params(("id" = String, Path, min_length = 8, max_length = 128)),
    responses(
        (status = 200, description = "Success", body = Envelope<OperationView>),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn get(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let (operation, steps) = state.store.operation_with_steps(&id).await?;
    Ok(ok(view(operation, steps)))
}

#[utoipa::path(
    get,
    path = "/api/v1/operations/{id}/events",
    operation_id = "watchOperation",
    summary = "Watch operation events",
    params(
        ("id" = String, Path, min_length = 8, max_length = 128),
        ("Last-Event-ID" = Option<String>, Header, nullable = false)
    ),
    responses(
        (status = 200, description = "Server-Sent Events", body = String, content_type = "text/event-stream"),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse))
    )
)]
pub(super) async fn events(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    state.store.operation(&id).await?;
    let last = last_event_id(&headers);
    let store = state.store;
    let stream = current_state_stream("operation", last, move || {
        let store = store.clone();
        let id = id.clone();
        async move {
            let (operation, steps) = store.operation_with_steps(&id).await.ok()?;
            let terminal = matches!(
                operation.state,
                WorkState::Succeeded | WorkState::Failed | WorkState::Cancelled
            );
            let data = serde_json::to_string(&view(operation, steps)).ok()?;
            Some(StateEventSnapshot {
                data,
                event: if terminal { "terminal" } else { "operation" },
                terminal,
            })
        }
    });
    Ok(Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keepalive"),
        )
        .into_response())
}

pub(super) fn view(operation: Operation, steps: Vec<OperationStep>) -> OperationView {
    OperationView {
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
    }
}
