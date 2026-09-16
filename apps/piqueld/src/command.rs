//! Shared process execution for Git and Docker, retaining only diagnostic tails.
use anyhow::{Context, bail};
use std::{collections::VecDeque, process::Stdio};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

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
            .spawn()
            .with_context(|| operation)?;
        let stdout = child.stdout.take().context("capture command stdout")?;
        let stderr = child.stderr.take().context("capture command stderr")?;
        let (status, stdout, stderr) = tokio::try_join!(
            async { child.wait().await.map_err(anyhow::Error::from) },
            Self::tail_recorded(stdout, log, piqueld_core::api::LogStream::Stdout),
            Self::tail_recorded(stderr, log, piqueld_core::api::LogStream::Stderr),
        )
        .with_context(|| operation)?;
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
}
