use super::{ApiError, ApiState, ok, openapi::ApiErrorResponse};
use axum::{
    extract::{Query, State, rejection::QueryRejection},
    http::{HeaderMap, StatusCode},
    response::{
        IntoResponse, Sse,
        sse::{Event as ServerEvent, KeepAlive},
    },
};
use piqueld_core::{
    Event,
    api::{Envelope, Page},
    observability::{EventFilter, EventScope},
};
use serde::Deserialize;

#[derive(Default, Deserialize, utoipa::IntoParams)]
#[serde(default, deny_unknown_fields)]
#[into_params(parameter_in=Query)]
pub(super) struct EventQuery {
    application_id: Option<String>,
    operation_id: Option<String>,
    attempt: Option<u64>,
    action_id: Option<String>,
    kind: Option<String>,
    error_code: Option<String>,
    errors_only: Option<bool>,
    scope: Option<EventScope>,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
    descending: Option<bool>,
    cursor: Option<String>,
    #[param(minimum = 1, maximum = 100)]
    limit: Option<usize>,
}
impl EventQuery {
    fn filter(&self) -> EventFilter {
        EventFilter {
            application_id: self.application_id.clone(),
            operation_id: self.operation_id.clone(),
            attempt: self.attempt,
            action_id: self.action_id.clone(),
            kind: self.kind.clone(),
            error_code: self.error_code.clone(),
            errors_only: self.errors_only.unwrap_or(false),
            scope: self.scope,
            since_ms: self.since_ms,
            until_ms: self.until_ms,
            descending: self.descending.unwrap_or(false),
        }
    }
    fn parse(query: Result<Query<Self>, QueryRejection>) -> Result<Self, ApiError> {
        query.map(|Query(q)| q).map_err(|_| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "pagination_invalid",
                "invalid event query",
            )
        })
    }
}
#[utoipa::path(get,path="/api/v1/events",operation_id="listEvents",params(EventQuery),responses((status=200,description="Structured history",body=Envelope<Page<Event>>),(status=400,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn list(
    State(state): State<ApiState>,
    query: Result<Query<EventQuery>, QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let query = EventQuery::parse(query)?;
    Ok(ok(state
        .filtered_events(
            &query.filter(),
            query.cursor.as_deref(),
            query.limit.unwrap_or(50),
        )
        .await?))
}
#[utoipa::path(get,path="/api/v1/events/stream",operation_id="streamEvents",params(EventQuery),responses((status=200,description="Resumable SSE; IDs are v1:<event-id>",body=String,content_type="text/event-stream"),(status=400,response=inline(ApiErrorResponse)),(status=410,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn stream(
    State(state): State<ApiState>,
    headers: HeaderMap,
    query: Result<Query<EventQuery>, QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let query = EventQuery::parse(query)?;
    if query.descending.unwrap_or(false) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "event streams require oldest-first ordering",
        ));
    }
    let cursor = headers
        .get("last-event-id")
        .map(|v| v.to_str().map(str::to_owned))
        .transpose()
        .map_err(|_| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "invalid Last-Event-ID",
            )
        })?
        .or(query.cursor.clone())
        .unwrap_or_else(|| "v1:0".into());
    let after = crate::store::Store::event_cursor(&cursor)?;
    let filter = query.filter();
    let batch_size = query.limit.unwrap_or(100);
    // Validate before sending headers; all later errors explicitly terminate the stream.
    let (initial, checkpoint) = state.stream_events(&filter, after, batch_size).await?;
    let stream = futures_util::stream::unfold(
        (
            state,
            filter,
            after,
            std::collections::VecDeque::from(initial),
            checkpoint,
            false,
        ),
        move |(state, filter, mut after, mut pending, mut checkpoint, done)| async move {
            if done {
                return None;
            }
            loop {
                if let Some(event) = pending.pop_front() {
                    after = event.id;
                    let item = ServerEvent::default()
                        .id(format!("v1:{after}"))
                        .event("event")
                        .json_data(event)
                        .unwrap_or_else(|_| {
                            ServerEvent::default()
                                .event("stream_error")
                                .data("serialization failed")
                        });
                    return Some((
                        Ok::<_, std::convert::Infallible>(item),
                        (state, filter, after, pending, checkpoint, false),
                    ));
                }
                if checkpoint > after {
                    // An ID-only message moves Last-Event-ID past events the filter
                    // excluded without dispatching an event to the client.
                    after = checkpoint;
                    return Some((
                        Ok(ServerEvent::default().id(format!("v1:{after}"))),
                        (state, filter, after, pending, checkpoint, false),
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                match state.stream_events(&filter, after, batch_size).await {
                    Ok((items, next)) => {
                        pending.extend(items);
                        checkpoint = next;
                    }
                    Err(error) => {
                        let kind = if matches!(
                            error,
                            crate::api::ApplicationError::Store(
                                crate::store::StoreError::HistoryExpired
                            )
                        ) {
                            "history_expired"
                        } else {
                            "stream_error"
                        };
                        return Some((
                            Ok(ServerEvent::default()
                                .event(kind)
                                .data("Reload history before reconnecting")),
                            (state, filter, after, pending, checkpoint, true),
                        ));
                    }
                }
            }
        },
    );
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
