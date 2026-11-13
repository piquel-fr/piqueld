//! Asynchronous operation inspection, and their timelines of steps.
use crate::{
    Client, ClientError, Operation,
    client::{EnvelopedCall, GeneratedCall},
    sse::{Messages, Received},
};
pub use piqueld_core::timeline::{Stage, Step, StepKey, StepState, Timeline};

impl Client {
    /// Fetches an asynchronous operation by identifier.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn operation(&self, id: &str) -> Result<Operation, ClientError> {
        self.generated.get_operation(id).data().await
    }

    /// Reads an operation's timeline: the steps it ran, is running, and has
    /// left.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn operation_timeline(&self, id: &str) -> Result<Timeline, ClientError> {
        self.generated.get_operation_timeline(id).data().await
    }

    /// Follows an operation's timeline as it changes. The stream has no
    /// request deadline, since it lasts as long as the caller follows it;
    /// it carries keep-alives every 15 seconds, so the caller can bound how
    /// long it waits for one.
    ///
    /// # Errors
    /// Returns [`ClientError`] when the stream cannot be opened, including a
    /// 503 when the daemon has too many streams open.
    pub async fn operation_timeline_stream(&self, id: &str) -> Result<TimelineStream, ClientError> {
        let body = self
            .streaming()
            .generated
            .stream_operation_timeline(id)
            .payload()
            .await?;
        Ok(TimelineStream {
            messages: Messages::new(body),
        })
    }
}

/// An operation's timeline as the daemon streams it: whole, at once, then
/// again each time it changes. Reconnecting sends it whole again.
pub struct TimelineStream {
    messages: Messages,
}

impl TimelineStream {
    /// The timeline as it changed; `Ok(None)` for a keep-alive, which the
    /// daemon sends every 15 seconds, so a caller can tell a quiet operation
    /// from a dropped stream; or `None` once the stream ended, as it does
    /// when the daemon shuts down. Cancel-safe.
    ///
    /// # Errors
    /// Returns a transport error when the connection fails, an exchange
    /// error when the daemon can no longer read history (`stream_error`),
    /// and a 403 API error with the daemon's reason once the caller lost
    /// access (`access_ended`). The stream ends after any of them.
    pub async fn next(&mut self) -> Option<Result<Option<Timeline>, ClientError>> {
        loop {
            let message = match self.messages.next().await? {
                Ok(Received::Message(message)) => message,
                Ok(Received::Comment) => return Some(Ok(None)),
                Err(error) => return Some(Err(error)),
            };
            return Some(match message.event.as_str() {
                "timeline" => serde_json::from_str(&message.data)
                    .map(Some)
                    .map_err(|source| ClientError::Decode { source }),
                "stream_error" => Err(ClientError::exchange(format!(
                    "the timeline stream failed: {}",
                    message.data
                ))),
                "access_ended" => Err(ClientError::api(
                    http::StatusCode::FORBIDDEN,
                    message.data.as_bytes(),
                )),
                _ => continue,
            });
        }
    }
}
