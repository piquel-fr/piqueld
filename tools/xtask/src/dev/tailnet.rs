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
use super::{Instance, RUNTIME_ROOT};
use crate::process::{Job, Shutdown};

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
}

impl Tailnet {
    pub(super) fn new() -> Result<Self> {
        Ok(Self {
            state: Instance::state_root()?.join("tailnet"),
            runtime: Path::new(RUNTIME_ROOT).join("tailnet"),
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
        ensure!(
            output.status.success(),
            "tailscale status failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let status: Status =
            serde_json::from_slice(&output.stdout).context("parse tailscale status")?;
        Ok(status
            .node
            .filter(|_| status.backend_state == "Running")
            .map(|node| node.dns_name.trim_end_matches('.').to_owned())
            .filter(|name| !name.is_empty()))
    }
}
