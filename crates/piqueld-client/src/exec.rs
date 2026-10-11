//! One-off commands streamed over a WebSocket. See [`piqueld_core::exec`].
//!
//! Native builds frame the WebSocket over a connection upgraded by reqwest;
//! browser builds use the browser's `WebSocket`, authenticated by the session
//! cookie. Dropping both the reader and the writer closes the connection,
//! which stops streaming without stopping the command.

use crate::{Client, ClientError, TransportFailure, client::invalid_request};
use piqueld_core::api::API_PREFIX;
use piqueld_core::exec::MAX_STDIN_BYTES;
pub use piqueld_core::exec::{
    ExecCommand, ExecCommandError, ExecFrame, ExecInput, ExecOutput, ExecRequest, TerminalSize,
};

#[cfg_attr(target_arch = "wasm32", path = "exec/browser.rs")]
#[cfg_attr(not(target_arch = "wasm32"), path = "exec/native.rs")]
mod socket;

/// Receives output messages from a running command.
pub struct ExecReader(socket::Reader);

/// Sends input messages to a running command.
pub struct ExecWriter(socket::Writer);

impl Client {
    /// Starts a command in a running task of one service of an environment.
    ///
    /// Natively, the client timeout bounds only the WebSocket handshake; the
    /// returned stream lasts until the command exits. Errors starting the
    /// command, such as `service_not_running`, arrive as [`ExecOutput::Failed`].
    /// # Errors
    /// Returns transport errors, or API errors rejecting the handshake. Browsers
    /// cannot read a refused handshake's response, so there it is a transport error.
    pub async fn exec(
        &self,
        id: &str,
        request: &ExecRequest,
    ) -> Result<(ExecReader, ExecWriter), ClientError> {
        let url = format!(
            "{}{API_PREFIX}/environments/{}/exec",
            self.generated.baseurl,
            progenitor_client::encode_path(id)
        );
        let request = serde_json::to_string(request).map_err(invalid_request)?;
        let (reader, writer) = socket::open(self, url, request).await?;
        Ok((ExecReader(reader), ExecWriter(writer)))
    }
}

impl ExecReader {
    /// Returns the next output message. The final one is [`ExecOutput::Exit`]
    /// or [`ExecOutput::Failed`]; `None` means the stream ended before it.
    /// # Errors
    /// Returns transport errors and malformed messages.
    pub async fn next(&mut self) -> Result<Option<ExecOutput>, ClientError> {
        match self.0.next().await? {
            Some(data) => ExecOutput::decode(&data)
                .map(Some)
                .map_err(|error| exchange(&error)),
            None => Ok(None),
        }
    }
}

impl ExecWriter {
    /// Sends one input message. Standard input larger than one message
    /// allows, such as a large paste, is split across several.
    /// # Errors
    /// Returns transport errors, including a command that already exited.
    pub async fn send(&mut self, frame: &ExecInput) -> Result<(), ClientError> {
        match frame {
            ExecInput::Stdin(data) => {
                for chunk in data.chunks(MAX_STDIN_BYTES) {
                    self.0
                        .send(&ExecInput::Stdin(chunk.to_vec()).encode())
                        .await?;
                }
                Ok(())
            }
            frame => self.0.send(&frame.encode()).await,
        }
    }
}

/// A failure of an established, or establishing, exec stream.
fn exchange(error: &impl std::fmt::Display) -> ClientError {
    ClientError::Transport {
        message: format!("exec stream failed: {error}"),
        kind: TransportFailure::Exchange,
    }
}
