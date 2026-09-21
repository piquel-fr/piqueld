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
    if after > 0 {
        state.check_event_resume(after).await?;
    }
    let filter = query.filter();
    // Validate before sending headers; all later errors explicitly terminate the stream.
    let initial = state
        .filtered_events(&filter, Some(&cursor), 100)
        .await?
        .items;
    let stream = futures_util::stream::unfold(
        (
            state,
            filter,
            cursor,
            std::collections::VecDeque::from(initial),
            false,
        ),
        |(state, filter, mut cursor, mut pending, done)| async move {
            if done {
                return None;
            }
            loop {
                if let Some(event) = pending.pop_front() {
                    cursor = format!("v1:{}", event.id);
                    let item = ServerEvent::default()
                        .id(&cursor)
                        .event("event")
                        .json_data(event)
                        .unwrap_or_else(|_| {
                            ServerEvent::default()
                                .event("stream_error")
                                .data("serialization failed")
                        });
                    return Some((
                        Ok::<_, std::convert::Infallible>(item),
                        (state, filter, cursor, pending, false),
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let after = crate::store::Store::event_cursor(&cursor).unwrap_or(0);
                let result = async {
                    if after > 0 {
                        state.check_event_resume(after).await?;
                    }
                    state.filtered_events(&filter, Some(&cursor), 100).await
                }
                .await;
                match result {
                    Ok(page) => pending.extend(page.items),
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
                            (state, filter, cursor, pending, true),
                        ));
                    }
                }
            }
        },
    );
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
