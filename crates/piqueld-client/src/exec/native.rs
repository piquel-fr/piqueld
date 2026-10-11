//! Exec `WebSocket` streams over connections upgraded by reqwest.

use super::exchange;
use crate::{Client, ClientError};
use futures_util::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
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

/// Receiving half: binary message payloads.
pub(super) struct Reader(SplitStream<Socket>);

/// Sending half: binary messages.
pub(super) struct Writer(SplitSink<Socket, Message>);

/// Completes the WebSocket handshake at `url` and sends `request` as the first message.
pub(super) async fn open(
    client: &Client,
    url: String,
    request: String,
) -> Result<(Reader, Writer), ClientError> {
    let key = generate_key();
    let response = client
        .execute(
            client
                .generated
                .client
                .get(url)
                .header(reqwest::header::CONNECTION, "upgrade")
                .header(reqwest::header::UPGRADE, "websocket")
                .header(reqwest::header::SEC_WEBSOCKET_VERSION, "13")
                .header(reqwest::header::SEC_WEBSOCKET_KEY, &key),
        )
        .await?;
    if response.status() != reqwest::StatusCode::SWITCHING_PROTOCOLS {
        return Err(Client::response_error(response).await);
    }
    let accept = response
        .headers()
        .get(reqwest::header::SEC_WEBSOCKET_ACCEPT);
    if accept.and_then(|value| value.to_str().ok()) != Some(&derive_accept_key(key.as_bytes())) {
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
    Ok((Reader(reader), Writer(writer)))
}

impl Reader {
    /// Returns the next binary payload, or `None` once the connection closed.
    pub(super) async fn next(&mut self) -> Result<Option<Vec<u8>>, ClientError> {
        while let Some(message) = self.0.next().await {
            match message.map_err(|error| exchange(&error))? {
                Message::Binary(data) => return Ok(Some(data.into())),
                Message::Close(_) => return Ok(None),
                _ => {}
            }
        }
        Ok(None)
    }
}

impl Writer {
    pub(super) async fn send(&mut self, data: &[u8]) -> Result<(), ClientError> {
        self.0
            .send(Message::binary(data.to_vec()))
            .await
            .map_err(|error| exchange(&error))
    }
}
