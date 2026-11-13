//! Shared process execution for Git and Docker, retaining only diagnostic tails.
use anyhow::Context;
use std::{collections::VecDeque, process::Stdio};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

/// Each command owns a process group, so cancellation also stops helpers such
/// as Git transports and Docker build plugins before the caller releases its slot.
/// Dropping the guard kills the group unless it was disarmed by clearing the PID.
struct ProcessGroup(Option<rustix::process::Pid>);

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        let Some(id) = self.0 else {
            return;
        };
        if let Err(error) = rustix::process::kill_process_group(id, rustix::process::Signal::KILL)
            && error != rustix::io::Errno::SRCH
        {
            tracing::warn!(?error, "failed to stop command process group");
        }
    }
}

/// Where a command streams its output as it runs, such as a build's
/// durable log.
#[async_trait::async_trait]
pub trait OutputLog: Send + Sync {
    /// Appends `bytes` the command wrote to `stream`. They end on a character
    /// boundary unless the command wrote invalid UTF-8.
    ///
    /// # Errors
    /// The output couldn't be recorded; the command then fails rather than
    /// silently losing it.
    async fn append(
        &self,
        bytes: &[u8],
        stream: piqueld_core::api::LogStream,
    ) -> anyhow::Result<()>;
}

/// Namespace for running external commands with bounded output capture.
pub struct LoggedCommand;

/// Typed facts are safe to expose; output tails remain internal diagnostics.
#[derive(Debug, thiserror::Error)]
#[error("{operation} failed ({status}):\nstdout: {stdout}\nstderr: {stderr}")]
pub struct CommandFailure {
    /// What the command was for, such as `clone Git repository`.
    pub operation: &'static str,
    /// How the command exited.
    pub status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

/// A command printed more than [`LoggedCommand::output`] reads.
#[derive(Debug, thiserror::Error)]
#[error("output exceeds {0} bytes")]
pub struct OutputLimit(pub usize);

impl LoggedCommand {
    /// Bytes of each output stream retained for failure diagnostics.
    const TAIL_BYTES: usize = 8192;

    /// Drain both streams concurrently with a fixed memory bound, streaming them
    /// into `log` when supplied. A non-zero exit returns a [`CommandFailure`] with
    /// the last `TAIL_BYTES` of each stream. The command runs in its own process
    /// group, which is killed if this future is dropped before the command exits.
    /// Error tails stay in internal diagnostics, never the public API response.
    ///
    /// # Errors
    /// The command can't start, exits unsuccessfully ([`CommandFailure`]), or
    /// its output can't be read or recorded.
    pub async fn run_recorded(
        command: &mut Command,
        operation: &'static str,
        log: Option<&dyn crate::command::OutputLog>,
    ) -> anyhow::Result<()> {
        let stdout =
            |stdout| Self::tail_recorded(stdout, log, piqueld_core::api::LogStream::Stdout);
        Self::run_in_group(command, operation, stdout, log)
            .await
            .map(drop)
    }

    /// Runs a command like [`Self::run_recorded`], without a log, and returns
    /// its whole standard output. More than `limit` bytes is an
    /// [`OutputLimit`] error.
    ///
    /// # Errors
    /// The errors of [`Self::run_recorded`], and [`OutputLimit`].
    pub async fn output(
        command: &mut Command,
        operation: &'static str,
        limit: usize,
    ) -> anyhow::Result<Vec<u8>> {
        let stdout = async |stdout: tokio::process::ChildStdout| {
            let mut output = Vec::new();
            stdout
                .take(u64::try_from(limit)?.saturating_add(1))
                .read_to_end(&mut output)
                .await?;
            if output.len() > limit {
                return Err(OutputLimit(limit).into());
            }
            Ok(output)
        };
        Self::run_in_group(command, operation, stdout, None).await
    }

    /// Runs a command in its own process group, reading its standard output
    /// with `read_stdout` and the last `TAIL_BYTES` of its standard error,
    /// streamed into `log` when supplied. Returns what `read_stdout` read.
    async fn run_in_group<F: Future<Output = anyhow::Result<Vec<u8>>>>(
        command: &mut Command,
        operation: &'static str,
        read_stdout: impl FnOnce(tokio::process::ChildStdout) -> F,
        log: Option<&dyn crate::command::OutputLog>,
    ) -> anyhow::Result<Vec<u8>> {
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0)
            .spawn()
            .with_context(|| operation)?;
        let mut group = ProcessGroup(Some(
            child
                .id()
                .and_then(|id| i32::try_from(id).ok())
                .and_then(rustix::process::Pid::from_raw)
                .context("capture command process group")?,
        ));
        let stdout = child.stdout.take().context("capture command stdout")?;
        let stderr = child.stderr.take().context("capture command stderr")?;
        // Keep the leader unreaped while draining pipes. Its reserved PID prevents
        // the process-group ID from being reused while cancellation can signal it.
        let (stdout, stderr) = tokio::try_join!(
            read_stdout(stdout),
            Self::tail_recorded(stderr, log, piqueld_core::api::LogStream::Stderr),
        )
        .with_context(|| operation)?;
        let status = child.wait().await.with_context(|| operation)?;
        // No await may intervene between reaping the leader and disarming.
        group.0 = None;
        if !status.success() {
            return Err(CommandFailure {
                operation,
                status,
                stdout: String::from_utf8_lossy(&stdout).into_owned(),
                stderr: String::from_utf8_lossy(&stderr).into_owned(),
            }
            .into());
        }
        Ok(stdout)
    }

    /// Reads a stream to the end, returning its last `TAIL_BYTES`.
    ///
    /// When `log` is supplied, output is appended as it arrives, holding back an
    /// incomplete trailing UTF-8 character until the next read or end of stream.
    async fn tail_recorded(
        mut stream: impl AsyncRead + Unpin,
        log: Option<&dyn crate::command::OutputLog>,
        source: piqueld_core::api::LogStream,
    ) -> anyhow::Result<Vec<u8>> {
        let mut tail = VecDeque::with_capacity(Self::TAIL_BYTES);
        let mut buffer = [0; 4096];
        let mut pending = Vec::new();
        loop {
            let read = stream.read(&mut buffer).await?;
            if read == 0 {
                if let Some(log) = log {
                    log.append(&pending, source).await?;
                }
                return Ok(tail.into());
            }
            if let Some(log) = log {
                pending.extend_from_slice(&buffer[..read]);
                let end = piqueld_core::api::BuildLogChunk::complete_prefix(&pending);
                log.append(&pending[..end], source).await?;
                pending.drain(..end);
            }
            let discard = (tail.len() + read).saturating_sub(Self::TAIL_BYTES);
            tail.drain(..discard);
            tail.extend(&buffer[..read]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Failures report only the end of each stream, read to completion.
    #[tokio::test]
    async fn drains_both_streams_and_preserves_non_utf8_failures() {
        let error = LoggedCommand::run_recorded(Command::new("sh").args(["-c", "head -c 131072 /dev/zero; head -c 131072 /dev/zero >&2; printf '\\377stdout tail'; printf '\\377stderr tail' >&2; exit 7"]), "fixture", None).await.unwrap_err().to_string();
        assert!(error.contains("exit status: 7"), "{error}");
        assert!(error.contains("�stdout tail"));
        assert!(error.contains("�stderr tail"));
        assert!(error.len() < LoggedCommand::TAIL_BYTES * 2 + 200);
    }

    #[tokio::test]
    async fn cancellation_kills_descendants_in_the_command_group() {
        // Exercise cancellation with both a live leader and an exited leader
        // whose descendant still owns its output streams.
        for ending in ["wait", "exit 0"] {
            let directory = tempfile::tempdir().unwrap();
            let pidfile = directory.path().join("descendant.pid");
            let path = pidfile.clone();
            let script = format!(r#"sh -c 'echo $$ > "$1"; exec sleep 60' child "$1" & {ending}"#);
            let command = tokio::spawn(async move {
                LoggedCommand::run_recorded(
                    Command::new("sh").args(["-c", &script, "parent", path.to_str().unwrap()]),
                    "cancellation fixture",
                    None,
                )
                .await
            });
            let pid = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if let Ok(contents) = tokio::fs::read_to_string(&pidfile).await
                        && let Ok(pid) = contents.trim().parse::<u32>()
                    {
                        break pid;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            command.abort();
            assert!(command.await.unwrap_err().is_cancelled());
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    // Orphaned descendants can briefly be zombies until PID 1 reaps
                    // them. They cannot continue work or retain build pipes/slots.
                    let status = tokio::fs::read_to_string(format!("/proc/{pid}/stat")).await;
                    if status
                        .as_ref()
                        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                        || status.as_ref().is_ok_and(|status| status.contains(") Z "))
                    {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("cancelled command descendant stopped");
        }
    }
}
