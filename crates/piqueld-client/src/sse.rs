//! Server-sent events read from a streamed response body.

use crate::{ClientError, client::MAX_RESPONSE_BODY_BYTES};
use futures_util::StreamExt;
use progenitor_client::ByteStream;

/// One server-sent event.
pub(crate) struct Message {
    /// Its `event` field, `message` without one.
    pub(crate) event: String,
    /// Its `data` lines, joined by newlines.
    pub(crate) data: String,
}

/// Reads server-sent events from a response body. Lines end in `\n` or
/// `\r\n`; comments, such as keep-alives, and other fields are skipped, and
/// a message may not exceed the response body limit.
pub(crate) struct Messages {
    body: ByteStream,
    /// Bytes received, not yet read as lines.
    buffer: Vec<u8>,
    /// The `event` field of the message read so far.
    event: Option<String>,
    /// The `data` of the message read so far, once it has some.
    data: Option<String>,
}

impl Messages {
    pub(crate) fn new(body: ByteStream) -> Self {
        Self {
            body,
            buffer: Vec::new(),
            event: None,
            data: None,
        }
    }

    /// The next message, or `None` once the body ended. Cancel-safe: bytes
    /// received stay buffered for the next call.
    pub(crate) async fn next(&mut self) -> Option<Result<Message, ClientError>> {
        loop {
            while let Some(end) = self.buffer.iter().position(|&byte| byte == b'\n') {
                let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                let line = match String::from_utf8(line) {
                    Ok(line) => line,
                    Err(source) => return Some(Err(ClientError::TextDecode { source })),
                };
                if line.is_empty() {
                    let event = self.event.take();
                    if let Some(data) = self.data.take() {
                        let event = event.unwrap_or_else(|| "message".to_owned());
                        return Some(Ok(Message { event, data }));
                    }
                    continue;
                }
                let (field, value) = line
                    .split_once(':')
                    .map_or((line.as_str(), ""), |(field, value)| {
                        (field, value.strip_prefix(' ').unwrap_or(value))
                    });
                match field {
                    "event" => self.event = Some(value.to_owned()),
                    "data" => {
                        if let Some(data) = &mut self.data {
                            data.push('\n');
                            data.push_str(value);
                        } else {
                            self.data = Some(value.to_owned());
                        }
                    }
                    _ => {}
                }
            }
            let chunk = match self.body.next().await? {
                Ok(chunk) => chunk,
                Err(error) => return Some(Err(ClientError::transport(error))),
            };
            let pending = self.buffer.len() + self.data.as_ref().map_or(0, String::len);
            if pending.saturating_add(chunk.len()) > MAX_RESPONSE_BODY_BYTES {
                return Some(Err(ClientError::exchange(format!(
                    "a server-sent event exceeded the {MAX_RESPONSE_BODY_BYTES}-byte limit"
                ))));
            }
            self.buffer.extend_from_slice(&chunk);
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::Messages;
    use bytes::Bytes;
    use progenitor_client::ByteStream;

    #[tokio::test]
    async fn messages_join_data_lines_across_chunks_and_skip_comments() {
        let chunks = [
            ": keep-alive\n\nevent: timeline\r\nda",
            "ta: {\"a\":\ndata:1}\nid: 7\n\n",
            "data: plain\n\nevent: ignored without data\n\n",
        ]
        .map(|chunk| Ok(Bytes::from_static(chunk.as_bytes())));
        let mut messages = Messages::new(ByteStream::new(Box::pin(futures_util::stream::iter(
            chunks,
        ))));
        let mut read = Vec::new();
        while let Some(message) = messages.next().await {
            let message = message.unwrap();
            read.push((message.event, message.data));
        }
        assert_eq!(
            read,
            [
                ("timeline".to_owned(), "{\"a\":\n1}".to_owned()),
                ("message".to_owned(), "plain".to_owned()),
            ]
        );
    }
}
