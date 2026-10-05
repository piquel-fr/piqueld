use super::{ApiError, ApiState, ok, openapi::ApiErrorResponse};
use crate::auth::Identity;
use axum::{
    Extension,
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

/// Event filters shared by the paged listing and the live stream.
#[derive(Default, Deserialize, utoipa::IntoParams)]
#[serde(default, deny_unknown_fields)]
#[into_params(parameter_in=Query)]
pub(super) struct EventQuery {
    /// Only include events of this application and all its environments.
    application_id: Option<String>,
    /// Only include events about this environment.
    environment_id: Option<String>,
    /// Only include events of this operation.
    operation_id: Option<String>,
    /// Only include events of this operation attempt.
    attempt: Option<u64>,
    /// Only include events of this runtime action.
    action_id: Option<String>,
    /// Only include events of this kind.
    kind: Option<String>,
    /// Only include failures with this error code.
    error_code: Option<String>,
    /// Only include diagnostic (failure) events.
    errors_only: Option<bool>,
    /// Only include environment-owned or daemon-owned history.
    scope: Option<EventScope>,
    /// Inclusive Unix millisecond lower bound.
    since_ms: Option<i64>,
    /// Inclusive Unix millisecond upper bound.
    until_ms: Option<i64>,
    /// Return newest events first; streams only support oldest first.
    descending: Option<bool>,
    /// `next_cursor` from a previous page; for streams, a `v1:<event-id>` SSE ID to
    /// resume after.
    cursor: Option<String>,
    /// Page size (defaults to 50), or stream batch size (defaults to 100).
    #[param(minimum = 1, maximum = 100)]
    limit: Option<usize>,
}
impl EventQuery {
    /// Extracts the store filter; pagination fields are handled separately.
    fn filter(&self) -> EventFilter {
        EventFilter {
            application_id: self.application_id.clone(),
            environment_id: self.environment_id.clone(),
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
    /// Maps a rejected query string to a generic 400.
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
/// Lists structured events.
///
/// Oldest first unless `descending=true`. Follow `next_cursor` to load more.
#[utoipa::path(get,path="/api/v1/events",operation_id="listEvents",params(EventQuery),responses((status=200,description="Structured history",body=Envelope<Page<Event>>),(status=400,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn list(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    query: Result<Query<EventQuery>, QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let query = EventQuery::parse(query)?;
    let visible = super::access::history(&identity)?;
    Ok(ok(state
        .filtered_events(
            &query.filter(),
            &visible,
            query.cursor.as_deref(),
            query.limit.unwrap_or(50),
        )
        .await?))
}
// The first batch is read before responding so request errors are still plain
// JSON; afterwards the stream polls the store every second.
/// Streams structured events as server-sent events.
///
/// Events arrive oldest first, resuming after the `Last-Event-ID` header or
/// `cursor` (from the beginning when both are absent). Invalid cursors fail with
/// 400 and pruned history with 410 before the stream opens. The stream emits:
/// - `event` messages with `id: v1:<event-id>` for each matching event;
/// - ID-only messages that advance the resume position past filtered-out events;
/// - a final `history_expired` or `stream_error` message, after which the
///   stream ends and history must be reloaded before reconnecting.
#[utoipa::path(get,path="/api/v1/events/stream",operation_id="streamEvents",params(EventQuery),responses((status=200,description="Resumable SSE; IDs are v1:<event-id>",body=String,content_type="text/event-stream"),(status=400,response=inline(ApiErrorResponse)),(status=410,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn stream(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    query: Result<Query<EventQuery>, QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let query = EventQuery::parse(query)?;
    let visible = super::access::history(&identity)?;
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
    let (initial, checkpoint) = state
        .stream_events(&filter, &visible, after, batch_size)
        .await?;
    let stream = futures_util::stream::unfold(
        (
            state,
            filter,
            after,
            std::collections::VecDeque::from(initial),
            checkpoint,
            false,
            visible,
        ),
        move |(state, filter, mut after, mut pending, mut checkpoint, done, visible)| async move {
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
                        (state, filter, after, pending, checkpoint, false, visible),
                    ));
                }
                if checkpoint > after {
                    // An ID-only message moves Last-Event-ID past events the filter
                    // excluded without dispatching an event to the client.
                    after = checkpoint;
                    return Some((
                        Ok(ServerEvent::default().id(format!("v1:{after}"))),
                        (state, filter, after, pending, checkpoint, false, visible),
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                match state
                    .stream_events(&filter, &visible, after, batch_size)
                    .await
                {
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
                            (state, filter, after, pending, checkpoint, true, visible),
                        ));
                    }
                }
            }
        },
    );
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
