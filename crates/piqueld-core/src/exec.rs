//! One-off command execution in a running service task.
//!
//! A client posts an [`ExecRequest`] with `Connection: Upgrade` and
//! `Upgrade: piqueld-exec.v1`. After `101 Switching Protocols`, the client
//! sends [`ExecInput`] frames and the daemon sends [`ExecOutput`] frames until
//! a final [`ExecOutput::Exit`] or [`ExecOutput::Failed`].
//!
//! Each frame is a one-byte tag, a big-endian `u32` payload length, then the
//! payload. Decoding is transport-independent: callers append received bytes to
//! a buffer and remove complete frames with [`ExecFrame::decode`].
//!
//! ```text
//! ExecInput   1 Stdin(bytes)   2 CloseStdin (empty)   3 Resize(u16 width, u16 height)
//! ExecOutput  1 Stdout(bytes)  2 Stderr(bytes)  3 Exit(i64)  4 Failed(ErrorBody JSON)
//! ```

use crate::{ServiceName, api::ErrorBody};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// `Upgrade` header value naming this frame protocol.
pub const EXEC_PROTOCOL: &str = "piqueld-exec.v1";

/// Largest accepted frame payload.
pub const MAX_FRAME_PAYLOAD: usize = 1024 * 1024;

/// Frame header size: the tag byte plus the `u32` payload length.
const HEADER_BYTES: usize = 5;

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

/// Malformed exec stream data.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ExecFrameError {
    /// The frame tag is not defined for this direction.
    #[error("unknown exec frame tag {0}")]
    UnknownTag(u8),
    /// The payload exceeds [`MAX_FRAME_PAYLOAD`].
    #[error("exec frame exceeds {MAX_FRAME_PAYLOAD} bytes")]
    TooLarge,
    /// The payload does not match its tag.
    #[error("exec frame payload is malformed")]
    Malformed,
}

/// A frame of the exec stream protocol in one direction.
pub trait ExecFrame: Sized {
    /// Appends this frame's encoding to `out`.
    fn encode(&self, out: &mut Vec<u8>);

    /// Removes and returns the first complete frame of `buffer`, or `None`
    /// until more bytes arrive.
    ///
    /// # Errors
    /// Returns [`ExecFrameError`] for oversized or malformed frames.
    fn decode(buffer: &mut Vec<u8>) -> Result<Option<Self>, ExecFrameError>;

    /// Returns this frame's encoding.
    #[must_use]
    fn encoded(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode(&mut out);
        out
    }
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
    /// Final frame: the session failed before reporting an exit code.
    Failed(ErrorBody),
}

/// Appends one frame to `out`.
///
/// # Panics
/// Panics if `payload` is longer than `u32::MAX` bytes.
fn write_frame(out: &mut Vec<u8>, tag: u8, payload: &[u8]) {
    let length = u32::try_from(payload.len()).expect("frame payloads are bounded by callers");
    out.push(tag);
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(payload);
}

/// Removes the first complete `(tag, payload)` pair from `buffer`, or returns
/// `None` until the whole frame has arrived. Fails with
/// [`ExecFrameError::TooLarge`] as soon as the header announces an oversized
/// payload.
fn take_frame(buffer: &mut Vec<u8>) -> Result<Option<(u8, Vec<u8>)>, ExecFrameError> {
    let Some(header) = buffer.first_chunk::<HEADER_BYTES>() else {
        return Ok(None);
    };
    let tag = header[0];
    let length = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if length > MAX_FRAME_PAYLOAD {
        return Err(ExecFrameError::TooLarge);
    }
    if buffer.len() < HEADER_BYTES + length {
        return Ok(None);
    }
    let payload = buffer[HEADER_BYTES..HEADER_BYTES + length].to_vec();
    buffer.drain(..HEADER_BYTES + length);
    Ok(Some((tag, payload)))
}

impl ExecFrame for ExecInput {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::Stdin(data) => write_frame(out, 1, data),
            Self::CloseStdin => write_frame(out, 2, &[]),
            Self::Resize(size) => {
                let [w1, w2] = size.width.to_be_bytes();
                let [h1, h2] = size.height.to_be_bytes();
                write_frame(out, 3, &[w1, w2, h1, h2]);
            }
        }
    }

    fn decode(buffer: &mut Vec<u8>) -> Result<Option<Self>, ExecFrameError> {
        let Some((tag, payload)) = take_frame(buffer)? else {
            return Ok(None);
        };
        Ok(Some(match (tag, payload.as_slice()) {
            (1, _) => Self::Stdin(payload),
            (2, []) => Self::CloseStdin,
            (3, &[w1, w2, h1, h2]) => Self::Resize(TerminalSize {
                width: u16::from_be_bytes([w1, w2]),
                height: u16::from_be_bytes([h1, h2]),
            }),
            (1..=3, _) => return Err(ExecFrameError::Malformed),
            (tag, _) => return Err(ExecFrameError::UnknownTag(tag)),
        }))
    }
}

impl ExecFrame for ExecOutput {
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::Stdout(data) => write_frame(out, 1, data),
            Self::Stderr(data) => write_frame(out, 2, data),
            Self::Exit(code) => write_frame(out, 3, &code.to_be_bytes()),
            Self::Failed(error) => write_frame(
                out,
                4,
                &serde_json::to_vec(error).expect("error bodies always serialize"),
            ),
        }
    }

    fn decode(buffer: &mut Vec<u8>) -> Result<Option<Self>, ExecFrameError> {
        let Some((tag, payload)) = take_frame(buffer)? else {
            return Ok(None);
        };
        Ok(Some(match tag {
            1 => Self::Stdout(payload),
            2 => Self::Stderr(payload),
            3 => Self::Exit(i64::from_be_bytes(
                payload.try_into().map_err(|_| ExecFrameError::Malformed)?,
            )),
            4 => Self::Failed(
                serde_json::from_slice(&payload).map_err(|_| ExecFrameError::Malformed)?,
            ),
            tag => return Err(ExecFrameError::UnknownTag(tag)),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_across_partial_reads() {
        let frames = [
            ExecInput::Stdin(b"hello".to_vec()),
            ExecInput::Resize(TerminalSize {
                width: 120,
                height: 40,
            }),
            ExecInput::CloseStdin,
        ];
        let bytes = frames
            .iter()
            .flat_map(ExecFrame::encoded)
            .collect::<Vec<_>>();
        let mut buffer = Vec::new();
        let mut decoded = Vec::new();
        for byte in bytes {
            buffer.push(byte);
            while let Some(frame) = ExecInput::decode(&mut buffer).unwrap() {
                decoded.push(frame);
            }
        }
        assert_eq!(decoded, frames);
        assert!(buffer.is_empty());

        let mut buffer = ExecOutput::Exit(-3).encoded();
        assert!(matches!(
            ExecOutput::decode(&mut buffer),
            Ok(Some(ExecOutput::Exit(-3)))
        ));
    }

    #[test]
    fn malformed_and_oversized_frames_are_rejected() {
        let mut oversized = vec![1];
        oversized.extend_from_slice(&(u32::try_from(MAX_FRAME_PAYLOAD).unwrap() + 1).to_be_bytes());
        assert_eq!(
            ExecInput::decode(&mut oversized),
            Err(ExecFrameError::TooLarge)
        );
        let mut resize = Vec::new();
        write_frame(&mut resize, 3, &[0, 1]);
        assert_eq!(
            ExecInput::decode(&mut resize),
            Err(ExecFrameError::Malformed)
        );
        let mut unknown = Vec::new();
        write_frame(&mut unknown, 9, &[]);
        assert_eq!(
            ExecInput::decode(&mut unknown),
            Err(ExecFrameError::UnknownTag(9))
        );
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
