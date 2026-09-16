//! Startup-only TCP listener selection and Tailscale discovery.

use super::{ListenMode, ServerConfig};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};
use tokio::{net::TcpListener, process::Command};

impl ServerConfig {
    /// Binds every configured TCP address before the daemon starts serving.
    /// Unavailable Tailscale is warning-only and is never retried.
    ///
    /// # Errors
    /// Any bind failure aborts startup and releases listeners already bound.
    pub async fn bind_tcp(&self) -> Result<Vec<TcpListener>> {
        let tailscale = if matches!(self.listen_mode, ListenMode::Tailscale | ListenMode::Both) {
            let mut command = Command::new("tailscale");
            command.args(["status", "--json", "--peers=false"]);
            match TailscaleStatus::discover(command).await {
                Ok(addresses) => addresses,
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"),
                        "Tailscale listening is enabled but unavailable; restart piqueld to retry");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        let mut listeners = Vec::new();
        for address in self.listen_addresses(tailscale) {
            listeners.push(
                TcpListener::bind(address)
                    .await
                    .with_context(|| format!("failed to bind HTTP API on {address}"))?,
            );
        }
        Ok(listeners)
    }

    fn listen_addresses(&self, tailscale: Vec<IpAddr>) -> Vec<SocketAddr> {
        let mut addresses = Vec::new();
        if matches!(self.listen_mode, ListenMode::Localhost | ListenMode::Both) {
            addresses.extend([
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(Ipv6Addr::LOCALHOST),
            ]);
        }
        if matches!(self.listen_mode, ListenMode::Tailscale | ListenMode::Both) {
            addresses.extend(tailscale);
        }
        addresses
            .into_iter()
            .map(|ip| SocketAddr::new(ip, self.port))
            .collect()
    }
}

#[derive(Deserialize)]
struct TailscaleStatus {
    #[serde(rename = "BackendState")]
    backend_state: String,
    #[serde(rename = "TailscaleIPs", default)]
    addresses: Vec<IpAddr>,
}

impl TailscaleStatus {
    async fn discover(mut command: Command) -> Result<Vec<IpAddr>> {
        let output =
            tokio::time::timeout(Duration::from_secs(5), command.kill_on_drop(true).output())
                .await
                .context("Tailscale discovery timed out after 5 seconds")?
                .context("failed to run tailscale status")?;
        ensure!(
            output.status.success(),
            "tailscale status failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Self::parse(&output.stdout)
    }

    fn parse(output: &[u8]) -> Result<Vec<IpAddr>> {
        let status: Self =
            serde_json::from_slice(output).context("invalid tailscale status JSON")?;
        ensure!(
            status.backend_state == "Running",
            "Tailscale is {}",
            status.backend_state
        );
        ensure!(
            !status.addresses.is_empty(),
            "Tailscale has no assigned addresses"
        );
        // Never turn malformed discovery output into a wildcard listener.
        ensure!(
            status
                .addresses
                .iter()
                .all(|ip| !ip.is_unspecified() && !ip.is_loopback() && !ip.is_multicast()),
            "Tailscale reported an invalid interface address"
        );
        let mut addresses = status.addresses;
        addresses.sort_unstable();
        addresses.dedup();
        Ok(addresses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_select_only_the_requested_interfaces() {
        let tailscale = vec![
            "100.100.1.2".parse().unwrap(),
            "fd7a:115c:a1e0::2".parse().unwrap(),
        ];
        for (mode, expected) in [
            (ListenMode::Off, vec![]),
            (ListenMode::Localhost, vec!["127.0.0.1:7845", "[::1]:7845"]),
            (
                ListenMode::Tailscale,
                vec!["100.100.1.2:7845", "[fd7a:115c:a1e0::2]:7845"],
            ),
            (
                ListenMode::Both,
                vec![
                    "127.0.0.1:7845",
                    "[::1]:7845",
                    "100.100.1.2:7845",
                    "[fd7a:115c:a1e0::2]:7845",
                ],
            ),
        ] {
            let config = ServerConfig {
                listen_mode: mode,
                ..ServerConfig::default()
            };
            assert_eq!(
                config.listen_addresses(tailscale.clone()),
                expected
                    .iter()
                    .map(|s| s.parse::<SocketAddr>().unwrap())
                    .collect::<Vec<_>>()
            );
            let unavailable = config.listen_addresses(Vec::new());
            assert!(unavailable.iter().all(|address| address.ip().is_loopback()));
        }
    }

    #[test]
    fn discovery_requires_running_state_and_valid_addresses() {
        let valid = br#"{"BackendState":"Running","TailscaleIPs":["100.100.1.2","fd7a:115c:a1e0::2","100.100.1.2"]}"#;
        assert_eq!(TailscaleStatus::parse(valid).unwrap().len(), 2);
        for output in [
            r#"{"BackendState":"Stopped","TailscaleIPs":["100.100.1.2"]}"#,
            r#"{"BackendState":"NeedsLogin"}"#,
            r#"{"BackendState":"Running","TailscaleIPs":[]}"#,
            r#"{"BackendState":"Running","TailscaleIPs":["0.0.0.0"]}"#,
            r#"{"BackendState":"Running","TailscaleIPs":["::"]}"#,
            r#"{"BackendState":"Running","TailscaleIPs":["bad"]}"#,
            "not JSON",
        ] {
            assert!(
                TailscaleStatus::parse(output.as_bytes()).is_err(),
                "accepted {output}"
            );
        }
    }

    #[tokio::test]
    async fn discovery_reports_missing_command_and_failed_exit() {
        assert!(
            TailscaleStatus::discover(Command::new("/nonexistent/piqueld-tailscale"))
                .await
                .unwrap_err()
                .to_string()
                .contains("failed to run")
        );
        let mut command = Command::new("sh");
        command.args(["-c", "echo unavailable >&2; exit 1"]);
        let error = TailscaleStatus::discover(command)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("unavailable"));
        assert!(error.contains("exit status"));
    }

    #[tokio::test]
    async fn discovery_reads_cli_output() {
        let mut command = Command::new("sh");
        command.args(["-c", r#"printf '%s' '{"BackendState":"Running","TailscaleIPs":["100.100.1.2","fd7a:115c:a1e0::2"]}'"#]);
        let addresses = TailscaleStatus::discover(command).await.unwrap();
        assert_eq!(
            addresses,
            vec![
                "100.100.1.2".parse::<IpAddr>().unwrap(),
                "fd7a:115c:a1e0::2".parse().unwrap()
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_timeout_stops_waiting_for_the_command() {
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 60"]);
        let error = TailscaleStatus::discover(command).await.unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn bind_conflict_is_fatal_and_releases_previous_listeners() {
        let occupied = TcpListener::bind("[::1]:0").await.unwrap();
        let port = occupied.local_addr().unwrap().port();
        let config = ServerConfig {
            listen_mode: ListenMode::Localhost,
            port,
            ..ServerConfig::default()
        };
        let error = config.bind_tcp().await.unwrap_err();
        assert!(error.to_string().contains(&format!("[::1]:{port}")));
        TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
    }
}
