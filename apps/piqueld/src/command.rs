//! Shared process execution for Git and Docker, retaining only diagnostic tails.
use anyhow::{Context, bail};
use std::{collections::VecDeque, process::Stdio};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

/// Each command owns a process group, so cancellation also stops helpers such
/// as Git transports and Docker build plugins before the caller releases its slot.
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

pub(crate) struct LoggedCommand;
impl LoggedCommand {
    const TAIL_BYTES: usize = 8192;

    /// Drain both streams concurrently with a fixed memory bound and no log file.
    /// Error tails stay in internal diagnostics, never the public API response.
    #[cfg(test)]
    pub(crate) async fn run(command: &mut Command, operation: &'static str) -> anyhow::Result<()> {
        Self::run_recorded(command, operation, None).await
    }
    pub(crate) async fn run_recorded(
        command: &mut Command,
        operation: &'static str,
        log: Option<&crate::build::BuildLog>,
    ) -> anyhow::Result<()> {
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
            Self::tail_recorded(stdout, log, piqueld_core::api::LogStream::Stdout),
            Self::tail_recorded(stderr, log, piqueld_core::api::LogStream::Stderr),
        )
        .with_context(|| operation)?;
        let status = child.wait().await.with_context(|| operation)?;
        // No await may intervene between reaping the leader and disarming.
        group.0 = None;
        if !status.success() {
            bail!(
                "{operation} failed ({status}):\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            );
        }
        Ok(())
    }

    #[cfg(test)]
    async fn tail(stream: impl AsyncRead + Unpin) -> anyhow::Result<Vec<u8>> {
        Self::tail_recorded(stream, None, piqueld_core::api::LogStream::Stdout).await
    }
    async fn tail_recorded(
        mut stream: impl AsyncRead + Unpin,
        log: Option<&crate::build::BuildLog>,
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
                let end = crate::build::BuildLog::complete_prefix(&pending);
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

    #[tokio::test]
    async fn retains_only_the_tail() {
        let mut output = vec![b'x'; LoggedCommand::TAIL_BYTES * 4];
        output.extend_from_slice(b"final diagnostic");
        let tail = LoggedCommand::tail(output.as_slice()).await.unwrap();
        assert_eq!(tail, output[output.len() - LoggedCommand::TAIL_BYTES..]);
    }

    #[tokio::test]
    async fn drains_both_streams_and_preserves_non_utf8_failures() {
        let error = LoggedCommand::run(
            Command::new("sh").args(["-c", "head -c 131072 /dev/zero; head -c 131072 /dev/zero >&2; printf '\\377stdout tail'; printf '\\377stderr tail' >&2; exit 7"]),
            "fixture",
        ).await.unwrap_err().to_string();
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
                LoggedCommand::run(
                    Command::new("sh").args(["-c", &script, "parent", path.to_str().unwrap()]),
                    "cancellation fixture",
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
