//! One-off commands streamed over a WebSocket. See [`piqueld_core::exec`].

use crate::{Client, ClientError, TransportFailure, client::invalid_request};
use futures_util::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};
use piqueld_core::api::API_PREFIX;
pub use piqueld_core::exec::{
    ExecCommand, ExecCommandError, ExecFrame, ExecInput, ExecOutput, ExecRequest, TerminalSize,
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        handshake::{client::generate_key, derive_accept_key},
        protocol::Role,
    },
};

type Socket = WebSocketStream<reqwest::Upgraded>;

/// Receives output messages from a running command.
pub struct ExecReader(SplitStream<Socket>);

/// Sends input messages to a running command.
pub struct ExecWriter(SplitSink<Socket, Message>);

impl Client {
    /// Starts a command in a running task of one application service.
    ///
    /// The client timeout bounds only the WebSocket handshake; the returned
    /// stream lasts until the command exits. Errors starting the command, such
    /// as `service_not_running`, arrive as [`ExecOutput::Failed`].
    /// # Errors
    /// Returns transport errors, or API errors rejecting the handshake.
    pub async fn exec(
        &self,
        id: &str,
        request: &ExecRequest,
    ) -> Result<(ExecReader, ExecWriter), ClientError> {
        let url = format!(
            "{}{API_PREFIX}/applications/{}/exec",
            self.generated.baseurl,
            progenitor_client::encode_path(id)
        );
        let request = serde_json::to_string(request).map_err(invalid_request)?;
        let key = generate_key();
        let response = self
            .execute(
                self.generated
                    .client
                    .get(url)
                    .header(reqwest::header::CONNECTION, "upgrade")
                    .header(reqwest::header::UPGRADE, "websocket")
                    .header(reqwest::header::SEC_WEBSOCKET_VERSION, "13")
                    .header(reqwest::header::SEC_WEBSOCKET_KEY, &key),
            )
            .await?;
        if response.status() != reqwest::StatusCode::SWITCHING_PROTOCOLS {
            return Err(Self::response_error(response).await);
        }
        let accept = response
            .headers()
            .get(reqwest::header::SEC_WEBSOCKET_ACCEPT);
        if accept.and_then(|value| value.to_str().ok()) != Some(&derive_accept_key(key.as_bytes()))
        {
            return Err(exchange(&"server did not accept the WebSocket key"));
        }
        let connection = response.upgrade().await.map_err(|error| exchange(&error))?;
        let (mut writer, reader) = WebSocketStream::from_raw_socket(connection, Role::Client, None)
            .await
            .split();
        writer
            .send(Message::text(request))
            .await
            .map_err(|error| exchange(&error))?;
        Ok((ExecReader(reader), ExecWriter(writer)))
    }
}

impl ExecReader {
    /// Returns the next output message. The final one is [`ExecOutput::Exit`]
    /// or [`ExecOutput::Failed`]; `None` means the stream ended before it.
    /// # Errors
    /// Returns transport errors and malformed messages.
    pub async fn next(&mut self) -> Result<Option<ExecOutput>, ClientError> {
        while let Some(message) = self.0.next().await {
            match message.map_err(|error| exchange(&error))? {
                Message::Binary(data) => {
                    return ExecOutput::decode(&data)
                        .map(Some)
                        .map_err(|error| exchange(&error));
                }
                Message::Close(_) => return Ok(None),
                _ => {}
            }
        }
        Ok(None)
    }
}

impl ExecWriter {
    /// Sends one input message.
    /// # Errors
    /// Returns transport errors, including a command that already exited.
    pub async fn send(&mut self, frame: &ExecInput) -> Result<(), ClientError> {
        self.0
            .send(Message::binary(frame.encode()))
            .await
            .map_err(|error| exchange(&error))
    }
}

fn exchange(error: &impl std::fmt::Display) -> ClientError {
    ClientError::Transport {
        message: format!("exec stream failed: {error}"),
        kind: TransportFailure::Exchange,
    }
}
