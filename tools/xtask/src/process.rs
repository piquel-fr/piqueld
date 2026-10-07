//! Child processes that xtask stops together with their descendants.

use std::io::Write;
use std::process::{ExitCode, ExitStatus};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rustix::process::{Pid, Signal, kill_process_group};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::signal::unix::{SignalKind, signal};

/// A child process leading its own process group. Terminal signals reach only
/// xtask, which stops the whole group, so no build script, test, or daemon
/// child outlives it. A job dropped without being waited for, such as on an
/// error, is killed.
pub struct Job {
    name: String,
    child: Child,
    group: Pid,
}

impl Job {
    pub fn spawn(command: &mut Command) -> Result<Self> {
        let name = command
            .as_std()
            .get_program()
            .to_string_lossy()
            .into_owned();
        let child = command
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("start {name}"))?;
        let id = child.id().context("the child has no process ID")?;
        let group = Pid::from_raw(i32::try_from(id)?).context("invalid process ID")?;
        Ok(Self { name, child, group })
    }

    pub async fn wait(&mut self) -> Result<ExitStatus> {
        self.child
            .wait()
            .await
            .with_context(|| format!("wait for {}", self.name))
    }

    /// Waits for the job to exit. When `limit` elapses or a shutdown signal
    /// arrives first, stops it, allowing 30 seconds to exit, and fails.
    pub async fn finish(
        mut self,
        limit: Option<Duration>,
        shutdown: &mut Shutdown,
    ) -> Result<ExitStatus> {
        let reason = tokio::select! {
            status = self.wait() => return status,
            () = async {
                match limit {
                    Some(limit) => tokio::time::sleep(limit).await,
                    None => std::future::pending().await,
                }
            } => "timed out",
            () = shutdown.requested() => "was interrupted",
        };
        let name = self.name.clone();
        self.stop(Duration::from_secs(30)).await?;
        bail!("{name} {reason}")
    }

    /// Asks the group to terminate, kills it after `grace`, and returns the
    /// leader's status. Descendants outliving the leader are killed too.
    pub async fn stop(mut self, grace: Duration) -> Result<ExitStatus> {
        self.signal(Signal::TERM);
        let status = if let Ok(status) = tokio::time::timeout(grace, self.child.wait()).await {
            status?
        } else {
            self.signal(Signal::KILL);
            self.wait().await?
        };
        self.signal(Signal::KILL);
        Ok(status)
    }

    /// Signals the group, which may already be gone.
    fn signal(&self, signal: Signal) {
        kill_process_group(self.group, signal).ok();
    }

    /// Copies the job's piped stderr to ours and, without color codes, to
    /// `log`, until the job closes it.
    pub fn tee_stderr(&mut self, mut log: std::fs::File) -> Result<()> {
        let stderr = self.child.stderr.take().context("stderr is not piped")?;
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = Vec::new();
            while reader
                .read_until(b'\n', &mut line)
                .await
                .is_ok_and(|n| n > 0)
            {
                std::io::stderr().write_all(&line).ok();
                log.write_all(&strip_color(&line)).ok();
                line.clear();
            }
        });
        Ok(())
    }
}

/// Removes ANSI escape sequences such as colors, `ESC [ ... m`.
fn strip_color(line: &[u8]) -> Vec<u8> {
    let mut plain = Vec::with_capacity(line.len());
    let mut bytes = line.iter().copied();
    while let Some(byte) = bytes.next() {
        if byte == 0x1b {
            // Skip the introducer and parameters through the final byte.
            bytes
                .by_ref()
                .skip(1)
                .find(|byte| (0x40..=0x7e).contains(byte));
        } else {
            plain.push(byte);
        }
    }
    plain
}

/// The exit code that reports `status` to our caller.
pub fn exit_code(status: ExitStatus) -> ExitCode {
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .map_or(ExitCode::FAILURE, ExitCode::from)
}

/// SIGINT, SIGTERM, and SIGHUP, each asking xtask to stop its jobs and clean
/// up before exiting.
pub struct Shutdown {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}

impl Shutdown {
    pub fn listen() -> Result<Self> {
        let listen = |kind| signal(kind).context("listen for signals");
        Ok(Self {
            interrupt: listen(SignalKind::interrupt())?,
            terminate: listen(SignalKind::terminate())?,
            hangup: listen(SignalKind::hangup())?,
        })
    }

    /// Resolves when a shutdown signal arrives.
    pub async fn requested(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
            _ = self.hangup.recv() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn strip_color_keeps_text() {
        assert_eq!(
            super::strip_color(b"\x1b[1m\x1b[92m   Compiling\x1b[0m piqueld\n"),
            b"   Compiling piqueld\n"
        );
    }
}
