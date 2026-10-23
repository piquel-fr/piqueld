//! The shared development node, `piqueld-dev.<tailnet>.ts.net`. One
//! `tailscaled`, logged in once with `just dev tailnet up`, serves every
//! instance over HTTPS on the instance's own port, so any number of instances
//! need a single node and certificate. It runs under its own supervisor and
//! outlives the instances; `just dev tailnet stop` stops it.

use std::fs;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use clap::Subcommand;
use serde::Deserialize;
use tokio::process::Command;

use super::supervisor::Supervisor;
use super::{Instance, LEGACY_RUNTIME_ROOT};
use crate::process::{Job, Shutdown, checked_stdout};

const HOSTNAME: &str = "piqueld-dev";
/// tailscaled normally opens its socket well within a second.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Subcommand)]
pub enum TailnetCommand {
    /// Start the node and log it in: `tailscale up --hostname=piqueld-dev ARGS`.
    #[command(disable_help_flag = true)]
    Up {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Stop the node; its instances are only on localhost until it restarts.
    Stop,
    /// Run the node in the foreground, as `up` and `just dev` do in the background.
    #[command(hide = true)]
    Run,
}

/// The fields read from `tailscale status --json`.
#[derive(Deserialize)]
struct Status {
    #[serde(rename = "BackendState")]
    backend_state: String,
    #[serde(rename = "Self")]
    node: Option<NodeStatus>,
}

#[derive(Deserialize)]
struct NodeStatus {
    #[serde(rename = "DNSName")]
    dns_name: String,
}

/// The shared node's state and runtime directories.
pub(super) struct Tailnet {
    state: PathBuf,
    runtime: PathBuf,
    /// Where older checkouts ran the node, with the same state directory.
    legacy_runtime: PathBuf,
}

impl Tailnet {
    pub(super) fn new() -> Result<Self> {
        Ok(Self {
            state: Instance::state_root()?.join("tailnet"),
            runtime: Instance::runtime_root()?.join("tailnet"),
            legacy_runtime: Path::new(LEGACY_RUNTIME_ROOT).join("tailnet"),
        })
    }

    pub(super) async fn run(&self, command: TailnetCommand) -> Result<()> {
        match command {
            TailnetCommand::Up { args } => self.up(&args).await,
            TailnetCommand::Stop => self.supervisor().stop().await,
            TailnetCommand::Run => self.supervise().await,
        }
    }

    /// The socket instances configure as `tailscale.socket`.
    pub(super) fn socket(&self) -> PathBuf {
        self.runtime.join("tailscaled.sock")
    }

    fn log(&self) -> PathBuf {
        self.runtime.join("tailscaled.log")
    }

    fn supervisor(&self) -> Supervisor {
        Supervisor::in_dir(&self.runtime)
    }

    /// The `tailscale` CLI, pointed at the node.
    fn cli(&self) -> Command {
        let mut command = Command::new("tailscale");
        command.arg(format!("--socket={}", self.socket().display()));
        command
    }

    /// Starts the node in the background unless it runs, and waits until its
    /// socket accepts connections.
    pub(super) async fn start(&self) -> Result<()> {
        self.stop_legacy().await?;
        Instance::create_private_dir(&self.runtime)?;
        let log = self.log();
        self.supervisor()
            .launch(&["dev", "tailnet", "run"], &log)
            .await
            .with_context(|| format!("start the tailnet node; see {}", log.display()))?;
        let started = Instant::now();
        while UnixStream::connect(self.socket()).is_err() {
            ensure!(
                self.supervisor().running().is_some(),
                "the tailnet node stopped; see {}",
                log.display()
            );
            ensure!(
                started.elapsed() < STARTUP_TIMEOUT,
                "tailscaled did not open its socket within {STARTUP_TIMEOUT:?}; see {}",
                log.display()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(())
    }

    /// Stops the node an older checkout runs from the legacy runtime
    /// directory: it shares this node's state directory, and two tailscaled
    /// on one state would fight over the node. Its dev.pid is only trusted in
    /// a directory that is the user's alone.
    async fn stop_legacy(&self) -> Result<()> {
        let supervisor = Supervisor::in_dir(&self.legacy_runtime);
        if supervisor.running().is_none() {
            return Ok(());
        }
        let legacy = self.legacy_runtime.display();
        ensure!(
            Instance::private_runtime_dir(&self.legacy_runtime),
            "a tailnet node runs from {legacy}, which is not a directory only you can access; \
             stop it, then retry"
        );
        supervisor
            .stop()
            .await
            .with_context(|| format!("stop the tailnet node in {legacy}"))?;
        eprintln!("stopped the tailnet node that an older checkout ran from {legacy}");
        Ok(())
    }

    /// Runs tailscaled in userspace networking, like the daemon's own node,
    /// until a signal stops it. Its logs go to tailscaled.log.
    async fn supervise(&self) -> Result<()> {
        Instance::create_private_dir(&self.state)?;
        Instance::create_private_dir(&self.runtime)?;
        let _claim = self.supervisor().claim().await?;
        let mut shutdown = Shutdown::listen()?;
        // A socket left by a killed tailscaled would look like a running node.
        if let Err(error) = fs::remove_file(self.socket())
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(error).context("remove the stale tailscaled socket");
        }
        let mut job = Job::spawn(
            Command::new("tailscaled")
                .arg("--tun=userspace-networking")
                .arg("--port=0")
                .arg("--no-logs-no-support")
                .arg(format!("--statedir={}", self.state.display()))
                .arg(format!("--socket={}", self.socket().display()))
                // Keeps tailscaled's log configuration out of the home directory.
                .env("TS_LOGS_DIR", &self.state),
        )?;
        let exited = tokio::select! {
            status = job.wait() => Some(status?),
            () = shutdown.requested() => None,
        };
        match exited {
            Some(status) => bail!("tailscaled exited ({status})"),
            None => job.stop(Duration::from_secs(5)).await.map(drop),
        }
    }

    /// Starts the node and logs it in. `--reset` makes the flags its complete
    /// preferences, as the daemon does for its own node.
    async fn up(&self, args: &[String]) -> Result<()> {
        self.start().await?;
        let status = self
            .cli()
            .args(["up", "--reset", &format!("--hostname={HOSTNAME}")])
            .args(args)
            .status()
            .await
            .context("run tailscale up")?;
        ensure!(status.success(), "tailscale up failed ({status})");
        let name = self
            .dns_name()
            .await?
            .context("the tailnet node is not logged in")?;
        eprintln!(
            "the tailnet node is up as {name}; new instances serve on it, and \
             `just dev config --force` moves an existing one"
        );
        Ok(())
    }

    /// The node's DNS name while it is logged in. A node that has logged in
    /// before is started, such as after a reboot.
    pub(super) async fn dns_name(&self) -> Result<Option<String>> {
        if self.supervisor().running().is_none() {
            if !self.state.join("tailscaled.state").exists() {
                return Ok(None);
            }
            self.start().await?;
        }
        let output = self
            .cli()
            .args(["status", "--json", "--peers=false"])
            .output()
            .await
            .context("run tailscale status")?;
        let status: Status = serde_json::from_str(&checked_stdout("tailscale status", output)?)
            .context("parse tailscale status")?;
        Ok(status
            .node
            .filter(|_| status.backend_state == "Running")
            .map(|node| node.dns_name.trim_end_matches('.').to_owned())
            .filter(|name| !name.is_empty()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A supervisor from an older checkout: `sleep` holds the lock on its
    /// dev.pid, through its standard input, until SIGTERM ends it.
    fn legacy_supervisor(runtime: &Path) -> std::process::Child {
        let pid_file = fs::File::create(runtime.join("dev.pid")).unwrap();
        pid_file.lock().unwrap();
        let child = std::process::Command::new("sleep")
            .arg("60")
            .stdin(pid_file.try_clone().unwrap())
            .spawn()
            .unwrap();
        fs::write(runtime.join("dev.pid"), child.id().to_string()).unwrap();
        child
    }

    #[tokio::test]
    async fn starting_stops_an_older_checkouts_node() {
        let temporary = tempfile::tempdir().unwrap();
        let root = Instance::own_private_dir(temporary.path().join("root")).unwrap();
        let tailnet = Tailnet {
            state: root.join("state"),
            runtime: root.join("runtime"),
            legacy_runtime: Instance::own_private_dir(root.join("legacy")).unwrap(),
        };
        tailnet.stop_legacy().await.unwrap();
        let mut legacy = legacy_supervisor(&tailnet.legacy_runtime);
        tailnet.stop_legacy().await.unwrap();
        assert!(
            Supervisor::in_dir(&tailnet.legacy_runtime)
                .running()
                .is_none()
        );
        legacy.wait().unwrap();

        // Another user could have planted a dev.pid in a shared directory.
        let shared = Tailnet {
            legacy_runtime: temporary.path().join("shared"),
            ..tailnet
        };
        fs::create_dir(&shared.legacy_runtime).unwrap();
        let mut planted = legacy_supervisor(&shared.legacy_runtime);
        assert!(shared.stop_legacy().await.is_err());
        planted.kill().unwrap();
        planted.wait().unwrap();
    }
}
