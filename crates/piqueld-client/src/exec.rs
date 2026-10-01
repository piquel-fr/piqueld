//! One-off commands streamed over an upgraded connection. See [`piqueld_core::exec`].

use crate::{Client, ClientError, TransportFailure};
use piqueld_core::api::API_PREFIX;
pub use piqueld_core::exec::{
    EXEC_PROTOCOL, ExecCommand, ExecCommandError, ExecFrame, ExecInput, ExecOutput, ExecRequest,
    TerminalSize,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};

/// Receives output frames from a running command.
pub struct ExecReader {
    reader: ReadHalf<reqwest::Upgraded>,
    buffer: Vec<u8>,
}

/// Sends input frames to a running command.
pub struct ExecWriter {
    writer: WriteHalf<reqwest::Upgraded>,
}

impl Client {
    /// Starts a command in a running task of one application service.
    ///
    /// The client timeout bounds only the upgrade request; the returned stream
    /// lasts until the command exits.
    /// # Errors
    /// Returns transport errors, or API errors such as `service_not_running`.
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
        let response = self
            .execute(
                self.generated
                    .client
                    .post(url)
                    .header(reqwest::header::CONNECTION, "upgrade")
                    .header(reqwest::header::UPGRADE, EXEC_PROTOCOL)
                    .json(request),
            )
            .await?;
        if response.status() != reqwest::StatusCode::SWITCHING_PROTOCOLS {
            return Err(Self::response_error(response).await);
        }
        let connection = response.upgrade().await.map_err(|error| exchange(&error))?;
        let (reader, writer) = tokio::io::split(connection);
        Ok((
            ExecReader {
                reader,
                buffer: Vec::new(),
            },
            ExecWriter { writer },
        ))
    }
}

impl ExecReader {
    /// Returns the next output frame. The final frame is [`ExecOutput::Exit`]
    /// or [`ExecOutput::Failed`]; `None` means the stream ended before it.
    /// # Errors
    /// Returns transport errors and malformed frames.
    pub async fn next(&mut self) -> Result<Option<ExecOutput>, ClientError> {
        loop {
            if let Some(frame) = ExecOutput::decode(&mut self.buffer).map_err(|e| exchange(&e))? {
                return Ok(Some(frame));
            }
            if self
                .reader
                .read_buf(&mut self.buffer)
                .await
                .map_err(|e| exchange(&e))?
                == 0
            {
                return Ok(None);
            }
        }
    }
}

impl ExecWriter {
    /// Sends one input frame.
    /// # Errors
    /// Returns transport errors, including a command that already exited.
    pub async fn send(&mut self, frame: &ExecInput) -> Result<(), ClientError> {
        self.writer
            .write_all(&frame.encoded())
            .await
            .map_err(|e| exchange(&e))?;
        self.writer.flush().await.map_err(|e| exchange(&e))
    }
}

/// Wraps a failure on the upgraded connection, including malformed frames, as
/// an exchange transport error.
fn exchange(error: &impl std::fmt::Display) -> ClientError {
    ClientError::Transport {
        message: format!("exec stream failed: {error}"),
        kind: TransportFailure::Exchange,
    }
}
