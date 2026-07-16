use std::time::Duration;

use super::{
    ApiError, ApiState, StateEventSnapshot, current_state_stream, last_event_id, ok,
    openapi::ApiErrorResponse,
};
use crate::store::{OperationRepository, WorkState};
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    response::{IntoResponse, Response, Sse, sse::KeepAlive},
};
use piqueld_client::{Envelope, OperationView};

#[utoipa::path(
    get,
    path = "/api/v1/operations/{id}",
    operation_id = "getOperation",
    summary = "Get an operation",
    params(("id" = String, Path, min_length = 8, max_length = 64)),
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
    Ok(ok(operation.view(steps)))
}

#[utoipa::path(
    get,
    path = "/api/v1/operations/{id}/events",
    operation_id = "watchOperation",
    summary = "Watch operation events",
    params(
        ("id" = String, Path, min_length = 8, max_length = 64),
        ("Last-Event-ID" = Option<String>, Header, nullable = false, description = "Last durable/current-state event ID received by the client."),
    ),
    responses(
        (status = 200, description = "Server-Sent Events with durable/current-state IDs and bounded replay reset events.", body = String, content_type = "text/event-stream"),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    ),
    extensions(
        ("x-sse-terminal-closes" = json!(true)),
        ("x-sse-keepalive-seconds" = json!(15))
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
    Ok(Sse::new(current_state_stream("operation", last, move || {
        let store = store.clone();
        let id = id.clone();
        async move {
            let (operation, steps) = match store.operation_with_steps(&id).await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    tracing::warn!(operation_id = %id, %error, "operation event stream store read failed");
                    return None;
                }
            };
            let terminal = matches!(
                operation.state,
                WorkState::Succeeded | WorkState::Failed | WorkState::Cancelled
            );
            let data = match serde_json::to_string(&operation.view(steps)) {
                Ok(data) => data,
                Err(error) => {
                    tracing::error!(operation_id = %id, %error, "operation event stream serialization failed");
                    return None;
                }
            };
            Some(StateEventSnapshot {
                data,
                event: if terminal { "terminal" } else { "operation" },
                terminal,
            })
        }
    }))
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keepalive"),
        )
        .into_response())
}
