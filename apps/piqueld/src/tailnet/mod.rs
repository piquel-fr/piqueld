//! The daemon's own tailnet node. It serves the website over HTTPS with the
//! tailnet-issued certificate, independently of the host's Tailscale daemon.
//!
//! piqueld supervises a dedicated `tailscaled` in userspace-networking mode and
//! drives it through the `tailscale` CLI, so both must be on `PATH`.

mod node;
mod proxy;

pub use node::{Node, NodeListener};
pub use proxy::ProxyListener;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{path::PathBuf, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::Command,
};

/// The `tailscale` CLI, pointed at the dedicated daemon's socket.
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
    /// reach the operator, and is included in the error on failure.
    async fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        let operation = args.first().copied().unwrap_or_default();
        let mut child = Command::new("tailscale")
            .arg(format!("--socket={}", self.socket.display()))
            .args(args)
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
