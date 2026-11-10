//! Operations: their state, and their timeline of steps, read once or
//! streamed live.
use super::{
    ApiError, ApiPath, ApiState,
    auth::TailnetSource,
    events::PACE,
    lease::{Ended, Lease},
    ok,
    openapi::ApiErrorResponse,
};
use crate::{api::Expensive, auth::Identity};
use axum::{
    Extension,
    extract::State,
    response::{
        IntoResponse, Sse,
        sse::{Event as ServerEvent, KeepAlive},
    },
};
use piqueld_core::Operation;
use piqueld_core::access::{AppPermission, Grants};
use piqueld_core::api::Envelope;
use piqueld_core::timeline::Timeline;
use std::convert::Infallible;

/// Requires `grants` to read operation `id`'s environment, returning the
/// operation.
async fn authorize(state: &ApiState, grants: &Grants, id: &str) -> Result<Operation, ApiError> {
    let operation = state.operation(id).await?;
    let owner = state.environment_owner(&operation.environment_id).await?;
    grants.require_change(
        &[AppPermission::Read],
        piqueld_store::EnvironmentOwner::target(owner.as_ref()),
    )?;
    Ok(operation)
}

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
    Ok(ok(authorize(&state, &identity.grants, &id).await?))
}

/// Gets an operation's timeline.
///
/// Its steps: those it ran and is running, from its events, then those
/// left, from its latest plan or, before it planned that far, from its
/// environment's previous successful operation. Each step has how long it
/// usually takes, from the environment's recent successful operations.
#[utoipa::path(
    get,
    path = "/api/v1/operations/{id}/timeline",
    operation_id = "getOperationTimeline",
    params(("id" = String, Path, min_length = 8, max_length = 128)),
    responses(
        (status = 200, description = "Success", body = Envelope<Timeline>),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 410, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn timeline(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiPath(id): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    authorize(&state, &identity.grants, &id).await?;
    Ok(ok(state.timeline(&id).await?))
}

/// Streams an operation's timeline as server-sent events.
///
/// Sends a `timeline` message with the whole timeline at once, then again
/// each time one of the operation's events changes it, at most every 100 ms.
/// A running step's progress between messages is the time since it started.
/// The stream ends with a `stream_error` message when history can no longer
/// be read, with an `access_ended` message whose data is the `ErrorBody` of
/// the request that would now be refused once the caller loses access, and
/// without a message when the daemon shuts down. Reconnecting sends the
/// whole timeline again.
#[utoipa::path(
    get,
    path = "/api/v1/operations/{id}/timeline/stream",
    operation_id = "streamOperationTimeline",
    params(("id" = String, Path, min_length = 8, max_length = 128)),
    responses(
        (status = 200, description = "SSE of `timeline` messages, each a whole `Timeline`", body = String, content_type = "text/event-stream"),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 410, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn stream_timeline(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    source: Option<TailnetSource>,
    ApiPath(id): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    authorize(&state, &identity.grants, &id).await?;
    let admitted = state.admit(Expensive::EventStream)?;
    // Subscribed first, so a write while the timeline is read wakes the stream.
    let written = state.written();
    let timeline = state.timeline(&id).await?;
    let follow = FollowTimeline {
        _admitted: admitted,
        lease: Lease::new(state.clone(), identity, source),
        state,
        written,
        read_at: tokio::time::Instant::now(),
        timeline,
        sent: false,
        done: false,
    };
    let stream = futures_util::stream::unfold(follow, |mut follow| async move {
        let item = follow.next().await?;
        Some((Ok::<_, Infallible>(item), follow))
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// A live timeline stream: the timeline as last sent, and the caller's access.
struct FollowTimeline {
    /// The stream's share of the event stream budget, while it is open.
    _admitted: tokio::sync::OwnedSemaphorePermit,
    state: ApiState,
    lease: Lease,
    /// Changes after each write to the store.
    written: tokio::sync::watch::Receiver<()>,
    /// When the store was last read.
    read_at: tokio::time::Instant,
    timeline: Timeline,
    /// Whether the timeline was sent since it last changed.
    sent: bool,
    /// Whether a final message was sent.
    done: bool,
}

impl FollowTimeline {
    /// The next message, or `None` once the stream ended. Access is rechecked
    /// before every message once due.
    async fn next(&mut self) -> Option<ServerEvent> {
        if self.done {
            return None;
        }
        loop {
            if self.lease.due() {
                let id = self.timeline.operation.id.clone();
                let checked = self
                    .lease
                    .recheck(|state: ApiState, identity: Identity| async move {
                        let grants = state.grants(identity.actor()).await?;
                        authorize(&state, &grants, &id).await.map(|_| ())
                    })
                    .await;
                match checked {
                    Ok(()) => {}
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
            if !self.sent {
                self.sent = true;
                return Some(
                    ServerEvent::default()
                        .event("timeline")
                        .json_data(&self.timeline)
                        .unwrap_or_else(|_| {
                            ServerEvent::default()
                                .event("stream_error")
                                .data("serialization failed")
                        }),
                );
            }
            self.lease.wait(self.written.changed()).await;
            tokio::time::sleep_until(self.read_at + PACE).await;
            self.read_at = tokio::time::Instant::now();
            let Ok(changed) = self.state.advance_timeline(&mut self.timeline).await else {
                self.done = true;
                return Some(
                    ServerEvent::default()
                        .event("stream_error")
                        .data("Reload the timeline before reconnecting"),
                );
            };
            self.sent &= !changed;
        }
    }
}
