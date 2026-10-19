//! Structured event history, paged or streamed as server-sent events.
use super::{
    ApiError, ApiQuery, ApiState, QueryParams,
    auth::TailnetSource,
    lease::{Ended, Lease},
    ok,
    openapi::ApiErrorResponse,
};
use crate::{auth::Identity, store::Visibility};
use axum::{
    Extension,
    extract::State,
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
use std::{collections::VecDeque, convert::Infallible};

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
}

impl QueryParams for EventQuery {
    const INVALID_CODE: &'static str = "pagination_invalid";
    const INVALID_MESSAGE: &'static str = "invalid event query";
}
/// Lists structured events.
///
/// Oldest first unless `descending=true`. Follow `next_cursor` to load more.
#[utoipa::path(get,path="/api/v1/events",operation_id="listEvents",params(EventQuery),responses((status=200,description="Structured history",body=Envelope<Page<Event>>),(status=400,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn list(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiQuery(query): ApiQuery<EventQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let visible = super::access::history(&identity.grants)?;
    Ok(ok(state
        .filtered_events(
            &query.filter(),
            &visible,
            query.cursor.as_deref(),
            query.limit.unwrap_or(crate::store::DEFAULT_PAGE_SIZE),
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
///   stream ends and history must be reloaded before reconnecting;
/// - a final `access_ended` message whose data is the `ErrorBody` of the
///   request that would now be refused, once the caller loses access (see
///   docs/authorization.md for how soon).
///
/// The stream also ends, without a message, when the daemon shuts down.
#[utoipa::path(get,path="/api/v1/events/stream",operation_id="streamEvents",params(EventQuery),responses((status=200,description="Resumable SSE; IDs are v1:<event-id>",body=String,content_type="text/event-stream"),(status=400,response=inline(ApiErrorResponse)),(status=410,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn stream(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    source: Option<TailnetSource>,
    headers: HeaderMap,
    ApiQuery(query): ApiQuery<EventQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let visible = super::access::history(&identity.grants)?;
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
    let batch_size = query.limit.unwrap_or(crate::store::MAX_PAGE_SIZE);
    // Validate before sending headers; all later errors explicitly terminate the stream.
    let (initial, checkpoint) = state
        .stream_events(&filter, &visible, after, batch_size)
        .await?;
    let follow = Follow {
        lease: Lease::new(state.clone(), identity, source),
        state,
        filter,
        visible,
        batch_size,
        after,
        pending: VecDeque::from(initial),
        checkpoint,
        done: false,
    };
    let stream = futures_util::stream::unfold(follow, |mut follow| async move {
        let item = follow.next().await?;
        Some((Ok::<_, Infallible>(item), follow))
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// A live event stream's position and access.
struct Follow {
    state: ApiState,
    lease: Lease,
    filter: EventFilter,
    /// What the caller may read, updated whenever its access is rechecked.
    visible: Visibility,
    batch_size: usize,
    /// ID of the last event sent.
    after: i64,
    /// Read events not yet sent.
    pending: VecDeque<Event>,
    /// Newest event ID read, matching or not.
    checkpoint: i64,
    /// Whether a final message was sent.
    done: bool,
}

impl Follow {
    /// The next message, or `None` once the stream ended. Access is rechecked
    /// before every message once due, so nothing read earlier is sent after
    /// the caller lost access to it.
    async fn next(&mut self) -> Option<ServerEvent> {
        if self.done {
            return None;
        }
        loop {
            if self.lease.due() {
                let checked = self
                    .lease
                    .recheck(|state: ApiState, identity: Identity| async move {
                        Ok(super::access::history(
                            &state.grants(identity.actor()).await?,
                        )?)
                    })
                    .await;
                match checked {
                    Ok(visible) if visible != self.visible => {
                        // Events read with the previous visibility are read again.
                        self.visible = visible;
                        self.pending.clear();
                        self.checkpoint = self.after;
                    }
                    Ok(_) => {}
                    Err(Ended::Shutdown) => return None,
                    Err(Ended::Refused(error)) => {
                        self.done = true;
                        return Some(
                            ServerEvent::default()
                                .event("access_ended")
                                .json_data(error.body())
                                .unwrap_or_else(|_| ServerEvent::default().event("access_ended")),
                        );
                    }
                }
            }
            if let Some(event) = self.pending.pop_front() {
                self.after = event.id;
                return Some(
                    ServerEvent::default()
                        .id(format!("v1:{}", self.after))
                        .event("event")
                        .json_data(event)
                        .unwrap_or_else(|_| {
                            ServerEvent::default()
                                .event("stream_error")
                                .data("serialization failed")
                        }),
                );
            }
            if self.checkpoint > self.after {
                // An ID-only message moves Last-Event-ID past events the filter
                // excluded without dispatching an event to the client.
                self.after = self.checkpoint;
                return Some(ServerEvent::default().id(format!("v1:{}", self.after)));
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            match self
                .state
                .stream_events(&self.filter, &self.visible, self.after, self.batch_size)
                .await
            {
                Ok((items, next)) => {
                    self.pending.extend(items);
                    self.checkpoint = next;
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
                    self.done = true;
                    return Some(
                        ServerEvent::default()
                            .event(kind)
                            .data("Reload history before reconnecting"),
                    );
                }
            }
        }
    }
}
