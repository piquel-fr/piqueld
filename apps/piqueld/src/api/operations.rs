use std::{sync::Arc, time::Duration};

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
};
use futures_util::stream;

use super::{ApiError, ApiState, EventStream, current_state_event_id, last_event_id, ok};
use crate::store::{OperationRepository, SqliteStore, WorkState};

pub(super) async fn get(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let (operation, steps) = state.store.operation_with_steps(&id).await?;
    Ok(ok(operation.view(steps)))
}

pub(super) async fn events(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    state.store.operation(&id).await?;
    let last = last_event_id(&headers);
    Ok(Sse::new(event_stream(state.store, id, last))
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keepalive"),
        )
        .into_response())
}

fn event_stream(store: Arc<SqliteStore>, id: String, last: Option<String>) -> EventStream {
    Box::pin(stream::unfold(
        (store, id, last, false, true),
        |(store, id, last, done, reconnect)| async move {
            if done {
                return None;
            }
            loop {
                tokio::time::sleep(Duration::from_millis(200)).await;
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
                let event_id = current_state_event_id("operation", &data);
                if last.as_deref() == Some(event_id.as_str()) {
                    if terminal {
                        return None;
                    }
                    continue;
                }
                if reconnect && last.is_some() {
                    let reset = Event::default()
                        .id(format!("reset:{event_id}"))
                        .event("replay_reset")
                        .data("{\"reason\":\"bounded_replay_exhausted\"}");
                    return Some((Ok(reset), (store, id, None, false, false)));
                }
                let event = Event::default()
                    .id(event_id.clone())
                    .event(if terminal { "terminal" } else { "operation" })
                    .data(data);
                return Some((Ok(event), (store, id, Some(event_id), terminal, false)));
            }
        },
    ))
}
