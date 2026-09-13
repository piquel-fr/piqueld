//! Bounded diagnostics for external commands.
use anyhow::{Context, bail};
use std::{
    io::{Read, Seek, SeekFrom},
    process::Stdio,
};
use tokio::process::Command;

pub(crate) struct LoggedCommand;
impl LoggedCommand {
    /// Execute without retaining unbounded build output in memory. Error tails
    /// stay in internal diagnostics, never the public API response.
    pub(crate) async fn run(command: &mut Command, operation: &'static str) -> anyhow::Result<()> {
        let mut log = tempfile::tempfile().context("create command log")?;
        let status = command
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log.try_clone()?)
            .kill_on_drop(true)
            .status()
            .await
            .with_context(|| operation)?;
        if !status.success() {
            log.seek(SeekFrom::End(-i64::try_from(
                log.metadata()?.len().min(8192),
            )?))?;
            let mut tail = String::new();
            log.take(8192)
                .read_to_string(&mut tail)
                .context("read command diagnostics")?;
            bail!("{operation} failed ({status}): {tail}");
        }
        Ok(())
    }
}
