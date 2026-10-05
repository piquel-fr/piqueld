//! `WebSocket` connections streaming one-off commands. See `piqueld_core::exec`.
use super::{ApiError, ApiPath, ApiState, decode_json, openapi::ApiErrorResponse};
use crate::{api::ExecSession, auth::Identity, docker::ExecIo};
use axum::{
    Extension,
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
    ApplicationId,
    exec::{ExecFrame, ExecInput, ExecOutput, ExecRequest, MAX_MESSAGE_BYTES},
};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tower_http::request_id::RequestId;

/// How long the daemon waits to deliver the final message and close.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

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

#[utoipa::path(get,path="/api/v1/applications/{id}/exec",operation_id="execApplicationCommand",params(("id"=String,Path)),
 description="Opens a WebSocket whose first message is a JSON ExecRequest; see piqueld_core::exec.",
 responses((status=101,description="Switched to a WebSocket streaming the command"),
 (status=400,response=inline(ApiErrorResponse)),(status=426,response=inline(ApiErrorResponse))))]
pub(super) async fn exec(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
    ExecUpgrade(upgrade): ExecUpgrade,
    identity: Option<Extension<Identity>>,
    request_id: Option<Extension<RequestId>>,
) -> Result<Response, ApiError> {
    let id = ApplicationId::parse(id)?;
    let account = identity.map_or_else(
        || "An unidentified caller".to_owned(),
        |Extension(identity)| identity.user.username,
    );
    let request_id =
        request_id.and_then(|Extension(id)| id.header_value().to_str().ok().map(str::to_owned));
    Ok(upgrade
        .max_message_size(MAX_MESSAGE_BYTES)
        .on_failed_upgrade(|error| tracing::warn!(?error, "exec connection upgrade failed"))
        .on_upgrade(move |socket| async move {
            let (mut writer, mut reader) = socket.split();
            let result = async {
                let request = read_request(&mut reader).await?;
                let session = state.exec(&id, &request, &account).await?;
                Ok(relay(session, reader, &mut writer).await)
            }
            .await;
            // `None` means the client left, so there is nothing to report.
            let last = match result.unwrap_or_else(|error| Some(Err(error))) {
                None => None,
                Some(Ok(code)) => Some(ExecOutput::Exit(code)),
                Some(Err(error)) => {
                    let mut body = error.body();
                    if let Some(request_id) = request_id {
                        body.request_id = request_id;
                    }
                    let diagnostic = error.diagnostic.map(|diagnostic| *diagnostic);
                    state
                        .record_failure(error.status, &mut body, diagnostic, Some(&id))
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
/// exits. Returns `None` when the command failed because the client left, since
/// that is neither a failure to record nor one anybody can receive.
async fn relay(
    session: ExecSession,
    mut reader: SplitStream<WebSocket>,
    writer: &mut SplitSink<WebSocket, Message>,
) -> Option<Result<i64, ApiError>> {
    let (input, input_rx) = mpsc::channel(16);
    let (output_tx, output) = mpsc::channel::<ExecOutput>(16);
    let (connected, disconnected) = oneshot::channel();
    // Input is read independently so a quiet client never blocks output.
    // Ending the read drops `connected`, which tells the relay the client is
    // gone. The read returns whether the client ended it.
    let reading = tokio::spawn(async move {
        let _connected = connected;
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
    });
    // Owning `output` here drops it if the client disconnects, which stops the
    // command stream. Returns whether every frame was delivered.
    let writing = async {
        let mut output = output;
        while let Some(frame) = output.recv().await {
            if writer
                .send(Message::Binary(frame.encode().into()))
                .await
                .is_err()
            {
                return false;
            }
        }
        true
    };
    let io = ExecIo {
        input: input_rx,
        output: output_tx,
        disconnected,
    };
    let (result, delivered) = tokio::join!(session.run(io), writing);
    reading.abort();
    let left = !delivered || matches!(reading.await, Ok(true));
    match result {
        Err(_) if left => None,
        result => Some(result.map_err(ApiError::from)),
    }
}
