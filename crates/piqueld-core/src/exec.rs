//! One-off command execution in a running service task.
//!
//! A client opens a WebSocket at `GET /api/v1/applications/{id}/exec` and
//! sends an [`ExecRequest`] as its first message, in JSON text. The client then
//! sends [`ExecInput`] messages and the daemon sends [`ExecOutput`] messages
//! until a final [`ExecOutput::Exit`] or [`ExecOutput::Failed`]. Clients end
//! input with [`ExecInput::CloseStdin`]; closing the connection stops the session.
//!
//! Every message after the request is binary: a one-byte tag, then the payload.

use crate::{ServiceName, api::ErrorBody};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Largest message the daemon accepts.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// Program and arguments to execute. The program must be non-empty.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct ExecCommand(Vec<String>);

/// Rejected [`ExecCommand`] input.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("command must name a non-empty program")]
pub struct ExecCommandError;

impl ExecCommand {
    /// Validates a program followed by its arguments.
    ///
    /// # Errors
    /// Returns [`ExecCommandError`] when no program is named.
    pub fn parse(command: Vec<String>) -> Result<Self, ExecCommandError> {
        if command.first().is_some_and(|program| !program.is_empty()) {
            Ok(Self(command))
        } else {
            Err(ExecCommandError)
        }
    }

    /// Program followed by its arguments.
    #[must_use]
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
}

impl TryFrom<Vec<String>> for ExecCommand {
    type Error = ExecCommandError;
    fn try_from(command: Vec<String>) -> Result<Self, Self::Error> {
        Self::parse(command)
    }
}

impl From<ExecCommand> for Vec<String> {
    fn from(command: ExecCommand) -> Self {
        command.0
    }
}

/// Terminal dimensions in character cells.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct TerminalSize {
    /// Columns.
    pub width: u16,
    /// Rows.
    pub height: u16,
}

/// Starts a command in one running task of a deployed service.
///
/// The command inherits the task's environment, secrets, mounts and networks.
/// History records the service, task and account, never the command.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecRequest {
    /// Logical service whose running task executes the command.
    pub service: ServiceName,
    /// Program followed by its arguments.
    #[schema(value_type = Vec<String>, min_items = 1)]
    pub command: ExecCommand,
    /// Forward client input frames to the command's standard input.
    #[serde(default)]
    pub stdin: bool,
    /// Allocate a pseudo-terminal of this initial size. Terminal output is
    /// reported as standard output.
    #[serde(default)]
    pub tty: Option<TerminalSize>,
}

/// Malformed exec message.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ExecFrameError {
    /// The tag is not defined for this direction.
    #[error("unknown exec message tag {0}")]
    UnknownTag(u8),
    /// The message is empty, or its payload does not match its tag.
    #[error("exec message is malformed")]
    Malformed,
}

/// A binary message of the exec protocol in one direction.
pub trait ExecFrame: Sized {
    /// Returns this message's binary encoding.
    #[must_use]
    fn encode(&self) -> Vec<u8>;

    /// Decodes one binary message.
    ///
    /// # Errors
    /// Returns [`ExecFrameError`] for malformed messages.
    fn decode(message: &[u8]) -> Result<Self, ExecFrameError>;
}

/// Client-to-daemon frames.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecInput {
    /// Bytes for standard input.
    Stdin(Vec<u8>),
    /// Standard input reached its end. Ignored with a terminal, because
    /// Docker would also end its output; disconnect to detach instead.
    CloseStdin,
    /// The client terminal changed size.
    Resize(TerminalSize),
}

/// Daemon-to-client frames.
#[derive(Clone, Debug)]
pub enum ExecOutput {
    /// Bytes the command wrote to standard output.
    Stdout(Vec<u8>),
    /// Bytes the command wrote to standard error.
    Stderr(Vec<u8>),
    /// Final frame: the command exited with this code.
    Exit(i64),
    /// Final frame: the session failed before reporting an exit code, with
    /// the HTTP status an equivalent request would have returned.
    Failed {
        /// HTTP status code, e.g. 409 for `service_not_running`.
        status: u16,
        /// Public error body.
        error: ErrorBody,
    },
}

/// Prefixes `payload` with its tag.
fn frame(tag: u8, payload: &[u8]) -> Vec<u8> {
    [&[tag], payload].concat()
}

impl ExecFrame for ExecInput {
    fn encode(&self) -> Vec<u8> {
        match self {
            Self::Stdin(data) => frame(1, data),
            Self::CloseStdin => frame(2, &[]),
            Self::Resize(size) => {
                let [w1, w2] = size.width.to_be_bytes();
                let [h1, h2] = size.height.to_be_bytes();
                frame(3, &[w1, w2, h1, h2])
            }
        }
    }

    fn decode(message: &[u8]) -> Result<Self, ExecFrameError> {
        let (&tag, payload) = message.split_first().ok_or(ExecFrameError::Malformed)?;
        Ok(match (tag, payload) {
            (1, data) => Self::Stdin(data.to_vec()),
            (2, []) => Self::CloseStdin,
            (3, &[w1, w2, h1, h2]) => Self::Resize(TerminalSize {
                width: u16::from_be_bytes([w1, w2]),
                height: u16::from_be_bytes([h1, h2]),
            }),
            (2 | 3, _) => return Err(ExecFrameError::Malformed),
            (tag, _) => return Err(ExecFrameError::UnknownTag(tag)),
        })
    }
}

impl ExecFrame for ExecOutput {
    fn encode(&self) -> Vec<u8> {
        match self {
            Self::Stdout(data) => frame(1, data),
            Self::Stderr(data) => frame(2, data),
            Self::Exit(code) => frame(3, &code.to_be_bytes()),
            Self::Failed { status, error } => frame(
                4,
                &[
                    &status.to_be_bytes()[..],
                    &serde_json::to_vec(error).expect("error bodies always serialize"),
                ]
                .concat(),
            ),
        }
    }

    fn decode(message: &[u8]) -> Result<Self, ExecFrameError> {
        let (&tag, payload) = message.split_first().ok_or(ExecFrameError::Malformed)?;
        Ok(match tag {
            1 => Self::Stdout(payload.to_vec()),
            2 => Self::Stderr(payload.to_vec()),
            3 => Self::Exit(i64::from_be_bytes(
                payload.try_into().map_err(|_| ExecFrameError::Malformed)?,
            )),
            4 => {
                let (status, error) = payload
                    .split_first_chunk()
                    .ok_or(ExecFrameError::Malformed)?;
                Self::Failed {
                    status: u16::from_be_bytes(*status),
                    error: serde_json::from_slice(error).map_err(|_| ExecFrameError::Malformed)?,
                }
            }
            tag => return Err(ExecFrameError::UnknownTag(tag)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        for message in [
            ExecInput::Stdin(b"hello".to_vec()),
            ExecInput::Resize(TerminalSize {
                width: 120,
                height: 40,
            }),
            ExecInput::CloseStdin,
        ] {
            assert_eq!(ExecInput::decode(&message.encode()), Ok(message));
        }
        assert!(matches!(
            ExecOutput::decode(&ExecOutput::Exit(-3).encode()),
            Ok(ExecOutput::Exit(-3))
        ));
    }

    #[test]
    fn malformed_messages_are_rejected() {
        assert_eq!(ExecInput::decode(&[]), Err(ExecFrameError::Malformed));
        assert_eq!(
            ExecInput::decode(&[3, 0, 1]),
            Err(ExecFrameError::Malformed)
        );
        assert_eq!(ExecInput::decode(&[9]), Err(ExecFrameError::UnknownTag(9)));
    }

    #[test]
    fn commands_must_name_a_program() {
        assert!(ExecCommand::parse(Vec::new()).is_err());
        assert!(ExecCommand::parse(vec![String::new()]).is_err());
        assert!(serde_json::from_str::<ExecRequest>(r#"{"service":"web","command":[]}"#).is_err());
        let request: ExecRequest =
            serde_json::from_str(r#"{"service":"web","command":["sh","-c",""]}"#).unwrap();
        assert_eq!(request.command.as_slice(), ["sh", "-c", ""]);
        assert!(!request.stdin && request.tty.is_none());
    }
}
