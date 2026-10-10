//! The daemon's own tailnet node. It serves the website over HTTPS with the
//! tailnet-issued certificate, independently of the host's Tailscale daemon.
//!
//! piqueld supervises a dedicated `tailscaled` in userspace-networking mode, or
//! shares a configured one, and drives it through the `tailscale` CLI, so the
//! CLI (and `tailscaled`, unless shared) must be on `PATH`.

mod node;
mod proxy;
mod whois;

pub use node::{Node, NodeListener};
pub use proxy::ProxyListener;
pub use whois::{TailnetLookup, Whois, WhoisSource};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::Command,
};

/// Allows certificate issuance time while bounding non-interactive commands.
const COMMAND_TIMEOUT: Duration = Duration::from_mins(2);

/// The `tailscale` CLI, pointed at the node's `tailscaled` socket.
#[derive(Clone)]
struct Cli {
    socket: PathBuf,
}

/// The fields piqueld reads from `tailscale status --json`.
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

impl Cli {
    /// Runs `tailscale <args>` to completion and returns its stdout. Stderr is
    /// logged as it arrives, which is how interactive login URLs from `up`
    /// reach the operator, and is included in the error on failure. Only `up`
    /// may wait indefinitely for interactive approval.
    async fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        let operation = args.first().copied().unwrap_or_default();
        Self::run_command(
            Command::new("tailscale")
                .arg(format!("--socket={}", self.socket.display()))
                .args(args),
            operation,
        )
        .await
    }

    async fn run_command(command: &mut Command, operation: &str) -> Result<Vec<u8>> {
        let output = Self::output(command, operation);
        if operation == "up" {
            output.await
        } else {
            tokio::time::timeout(COMMAND_TIMEOUT, output)
                .await
                .with_context(|| {
                    format!("tailscale {operation} timed out after {COMMAND_TIMEOUT:?}")
                })?
        }
    }

    /// Dropping this future on timeout or shutdown kills the CLI child.
    async fn output(command: &mut Command, operation: &str) -> Result<Vec<u8>> {
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("failed to run tailscale")?;
        let mut stdout = child.stdout.take().context("capture tailscale stdout")?;
        let stderr = child.stderr.take().context("capture tailscale stderr")?;
        let (stdout, stderr) = tokio::try_join!(
            async {
                let mut output = Vec::new();
                stdout.read_to_end(&mut output).await.map(|_| output)
            },
            async {
                let (mut lines, mut output) = (BufReader::new(stderr).lines(), String::new());
                while let Some(line) = lines.next_line().await? {
                    let line = line.trim();
                    if !line.is_empty() {
                        tracing::info!("tailscale {operation}: {line}");
                        output.push_str(line);
                        output.push('\n');
                    }
                }
                Ok(output)
            },
        )
        .with_context(|| format!("failed to read tailscale {operation} output"))?;
        let status = child.wait().await.context("failed to wait for tailscale")?;
        ensure!(
            status.success(),
            "tailscale {operation} failed ({status}): {}",
            stderr.trim()
        );
        Ok(stdout)
    }

    async fn status(&self) -> Result<Status> {
        serde_json::from_slice(&self.run(&["status", "--json", "--peers=false"]).await?)
            .context("invalid tailscale status")
    }

    /// Fetches the certificate for the node's name and returns its expiry.
    /// tailscaled caches it in the node state and renews it before expiry.
    async fn certificate_expiry_ms(&self, dns_name: &str) -> Result<i64> {
        let pem = self
            .run(&["cert", "--cert-file=-", dns_name])
            .await
            .context("failed to obtain the tailnet HTTPS certificate; enable HTTPS certificates for the tailnet")?;
        let (_, pem) = x509_parser::pem::parse_x509_pem(&pem)
            .map_err(|error| anyhow::anyhow!("invalid certificate PEM: {error}"))?;
        let leaf = pem
            .parse_x509()
            .map_err(|error| anyhow::anyhow!("invalid certificate: {error}"))?;
        Ok(leaf.validity().not_after.timestamp().saturating_mul(1000))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn noninteractive_commands_time_out_but_login_can_wait() {
        for operation in ["status", "cert", "serve", "up"] {
            let mut command = Command::new("sh");
            command.args(["-c", "exec sleep 600"]);
            let result = tokio::time::timeout(
                COMMAND_TIMEOUT * 2,
                Cli::run_command(&mut command, operation),
            )
            .await;
            if operation == "up" {
                assert!(result.is_err(), "interactive login must keep waiting");
            } else {
                let error = result.unwrap().unwrap_err().to_string();
                assert!(
                    error.contains(&format!("tailscale {operation} timed out")),
                    "{error}"
                );
            }
        }
    }

    #[test]
    fn status_reads_login_state_and_node_name() {
        let status: Status = serde_json::from_str(
            r#"{"BackendState":"Running","Self":{"DNSName":"piqueld.tail.ts.net."}}"#,
        )
        .unwrap();
        assert_eq!(status.backend_state, "Running");
        assert_eq!(status.node.unwrap().dns_name, "piqueld.tail.ts.net.");
    }
}
