//! `WebSocket` connections streaming one-off commands. See `piqueld_core::exec`.
use super::{
    ApiError, ApiPath, ApiState,
    access::Audit,
    auth::{FreshTailnet, TailnetSource, binding_mismatch},
    decode_json,
    lease::{Ended, Lease},
    openapi::ApiErrorResponse,
};
use crate::{api::ExecSession, auth::Identity, docker::ExecIo};
use axum::{
    Extension,
    body::Bytes,
    extract::{
        FromRequestParts, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{StatusCode, request::Parts},
    response::Response,
};
use futures_util::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};
use piqueld_core::{
    EnvironmentId,
    exec::{ExecFrame, ExecInput, ExecOutput, ExecRequest, MAX_MESSAGE_BYTES},
};
use std::time::Duration;
use tokio::{sync::mpsc, time::Instant};
use tokio_util::sync::CancellationToken;

/// How long the daemon waits to deliver the final message and close.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the daemon pings a client, which notices one that left within
/// two intervals even while it is not reading the client's input.
const PING_INTERVAL: Duration = Duration::from_secs(5);

/// A WebSocket upgrade whose rejections use the API's JSON errors.
pub(super) struct ExecUpgrade(WebSocketUpgrade);

impl<S: Send + Sync> FromRequestParts<S> for ExecUpgrade {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        WebSocketUpgrade::from_request_parts(parts, state)
            .await
            .map(Self)
            .map_err(|_| {
                ApiError::new(
                    StatusCode::UPGRADE_REQUIRED,
                    "upgrade_required",
                    "Open a WebSocket to stream a command",
                )
            })
    }
}

#[utoipa::path(get,path="/api/v1/environments/{id}/exec",operation_id="execEnvironmentCommand",params(("id"=String,Path)),
 description="Opens a WebSocket whose first message is a JSON ExecRequest; see piqueld_core::exec.",
 responses((status=101,description="Switched to a WebSocket streaming the command"),
 (status=400,response=inline(ApiErrorResponse)),(status=426,response=inline(ApiErrorResponse))))]
pub(super) async fn exec(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
    ExecUpgrade(upgrade): ExecUpgrade,
    Extension(identity): Extension<Identity>,
    Extension(audit): Extension<Audit>,
    tailnet: Option<Extension<piqueld_core::tailnet::TailnetPeer>>,
    source: Option<TailnetSource>,
) -> Result<Response, ApiError> {
    let mut tailnet = tailnet.map(|Extension(peer)| peer);
    let id = EnvironmentId::parse(id)?;
    let request_id = audit.request_id().map(str::to_owned);
    Ok(upgrade
        .max_message_size(MAX_MESSAGE_BYTES)
        .on_failed_upgrade(|error| tracing::warn!(?error, "exec connection upgrade failed"))
        .on_upgrade(move |socket| async move {
            let (mut writer, mut reader) = socket.split();
            let result = async {
                let request = read_request(&mut reader).await?;
                // Like grants, a tailnet binding is rechecked once the command
                // arrives, against the peer as it is now.
                if let Some(fresh) = FreshTailnet::check(&identity, source.as_ref()).await {
                    let matches = fresh.matches(&identity);
                    tailnet = fresh.0;
                    if !matches {
                        return Err(binding_mismatch());
                    }
                }
                let caller = identity.actor();
                let session = state.exec(caller, &id, &request, &identity.who()).await?;
                // The command ends as soon as its caller may no longer start it.
                let lease = Lease::new(state.clone(), identity.clone(), source);
                let environment = &id;
                let ended = lease.ended(move |state: ApiState, identity: Identity| async move {
                    Ok(state.may_exec(identity.actor(), environment).await?)
                });
                Ok(relay(session, reader, &mut writer, ended).await)
            }
            .await;
            // `None` means the client left, so there is nothing to report.
            let last = match result.unwrap_or_else(|error| Some(Err(error))) {
                None => None,
                Some(Ok(code)) => Some(ExecOutput::Exit(code)),
                Some(Err(error)) => {
                    // Access is rechecked once the command arrives, after
                    // the upgrade was audited as allowed.
                    if error.denied.is_some() || error.status == StatusCode::UNAUTHORIZED {
                        let tailnet = tailnet.as_ref();
                        audit.refused_later(&state, identity.clone(), tailnet, &error);
                    }
                    let mut body = error.body();
                    if let Some(request_id) = request_id {
                        body.request_id = request_id;
                    }
                    let diagnostic = error.diagnostic.map(|diagnostic| *diagnostic);
                    let caller = identity.actor();
                    state
                        .record_failure(
                            error.status,
                            &mut body,
                            diagnostic,
                            Some(&id),
                            caller.attribution(),
                        )
                        .await;
                    Some(ExecOutput::Failed {
                        status: error.status.as_u16(),
                        error: body,
                    })
                }
            };
            // Closing also sends the reply to a client's Close. It is bounded
            // so a client that stopped reading cannot hold the connection.
            let finish = async {
                if let Some(last) = last {
                    let _ = writer.send(Message::Binary(last.encode().into())).await;
                }
                let _ = writer.close().await;
            };
            let _ = tokio::time::timeout(CLOSE_TIMEOUT, finish).await;
        }))
}

/// Reads the first message, which names the command to start.
async fn read_request(reader: &mut SplitStream<WebSocket>) -> Result<ExecRequest, ApiError> {
    loop {
        match reader.next().await {
            Some(Ok(Message::Text(text))) => return decode_json(text.as_bytes()),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            _ => {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "exec_request_missing",
                    "The first exec message must be a JSON ExecRequest",
                ));
            }
        }
    }
}

/// Relays messages between the client and the running command until it
/// exits, the client leaves, or `ended` resolves. Returns `None` when the
/// command failed because the client left, since that is neither a failure to
/// record nor one anybody can receive, and when the daemon shuts down.
async fn relay(
    session: ExecSession,
    mut reader: SplitStream<WebSocket>,
    writer: &mut SplitSink<WebSocket, Message>,
    ended: impl Future<Output = Ended>,
) -> Option<Result<i64, ApiError>> {
    let (input, input_rx) = mpsc::channel(16);
    let (output_tx, mut output) = mpsc::channel::<ExecOutput>(16);
    // Cancelled to stop the command: the client left, could not be written
    // to, or lost access, or the daemon is shutting down.
    let stop = CancellationToken::new();
    // Input is read independently so a quiet client never blocks output.
    // The read returns whether the client ended it, which stops the command.
    let reading = tokio::spawn({
        let stop = stop.clone();
        async move {
            let left = read_input(&mut reader, input).await;
            if left {
                stop.cancel();
            }
            left
        }
    });
    // Input the command does not read stops the reading above, so pings
    // notice a client that left while it waits. Returns whether every frame
    // was delivered.
    let writing = async {
        let mut ping = tokio::time::interval_at(Instant::now() + PING_INTERVAL, PING_INTERVAL);
        loop {
            let message = tokio::select! {
                frame = output.recv() => match frame {
                    Some(frame) => Message::Binary(frame.encode().into()),
                    None => return true,
                },
                _ = ping.tick() => Message::Ping(Bytes::new()),
            };
            let sent = tokio::select! {
                sent = writer.send(message) => sent.is_ok(),
                () = stop.cancelled() => false,
            };
            if !sent {
                stop.cancel();
                return false;
            }
        }
    };
    let io = ExecIo {
        input: input_rx,
        output: output_tx,
        stop: stop.clone(),
    };
    let running = async {
        let finished = tokio::join!(session.run(io), writing);
        stop.cancel();
        finished
    };
    let watching = async {
        tokio::select! {
            ended = ended => {
                stop.cancel();
                Some(ended)
            }
            () = stop.cancelled() => None,
        }
    };
    let ((result, delivered), ended) = tokio::join!(running, watching);
    reading.abort();
    match ended {
        Some(Ended::Refused(error)) => return Some(Err(error)),
        Some(Ended::Shutdown) => return None,
        None => {}
    }
    let left = !delivered || matches!(reading.await, Ok(true));
    match result {
        Err(_) if left => None,
        result => Some(result.map_err(ApiError::from)),
    }
}

/// Forwards the client's input to `input` until the client ends it or the
/// command no longer reads it. Returns whether the client ended it.
async fn read_input(reader: &mut SplitStream<WebSocket>, input: mpsc::Sender<ExecInput>) -> bool {
    while let Some(Ok(message)) = reader.next().await {
        let data = match message {
            Message::Binary(data) => data,
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Text(_) | Message::Close(_) => return true,
        };
        match ExecInput::decode(&data) {
            Ok(frame) => {
                if input.send(frame).await.is_err() {
                    return false;
                }
            }
            Err(error) => {
                tracing::warn!(%error, "rejected exec input");
                return true;
            }
        }
    }
    true
}
