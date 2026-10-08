//! A dedicated `tailscaled` owns the node. It terminates HTTPS on port 443 with
//! the tailnet certificate and forwards plain TCP, prefixed with a PROXY v2
//! header, to a loopback listener that the website is served on.

use super::{Cli, ProxyListener};
use crate::config::DaemonConfig;
use anyhow::{Context, Result, bail, ensure};
use axum::serve::{ListenerExt, TapIo};
use piqueld_core::api::TailnetStatus;
use std::{
    net::{Ipv4Addr, SocketAddr},
    os::unix::fs::DirBuilderExt,
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::{TcpListener, TcpStream},
    process::{Child, Command},
    sync::watch,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

const HTTPS_PORT: u16 = 443;
/// tailscaled renews certificates itself; refreshing keeps the reported
/// certificate expiry and login state current.
const REFRESH_INTERVAL: Duration = Duration::from_mins(1);
/// tailscaled normally opens its socket well within a second.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long after tailscaled exits a shutdown still explains the exit.
const SHUTDOWN_RACE: Duration = Duration::from_secs(1);

/// A logged-in tailnet node forwarding its HTTPS port to piqueld.
pub struct Node {
    daemon: Child,
    monitor: Monitor,
    listener: TcpListener,
    address: SocketAddr,
}

/// The node's HTTPS listener. The no-op `tap_io` wrapper is what lets axum
/// supply `ConnectInfo<SocketAddr>`, which authentication throttling needs;
/// axum otherwise derives it only for TCP listeners.
pub type NodeListener = TapIo<ProxyListener, fn(&mut TcpStream)>;

impl Node {
    /// Starts the node when `tailscale.enabled` is set and waits until it is
    /// logged in, holds a certificate, and forwards port 443 to piqueld. An
    /// unset `auth.public_url` becomes the node's HTTPS origin.
    ///
    /// Without an auth key or saved state, the node needs an interactive
    /// login; the login URL is logged and startup waits for it.
    ///
    /// # Errors
    /// Returns state directory, tailscaled, login, certificate, or forwarding
    /// errors. HTTPS certificates must be enabled for the tailnet.
    pub async fn join(config: &mut DaemonConfig) -> Result<Option<Self>> {
        if !config.tailscale.enabled {
            return Ok(None);
        }
        let dir = config.server.tailscale_dir();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&dir)
            .with_context(|| format!("failed to create tailnet state {}", dir.display()))?;
        let cli = Cli {
            socket: dir.join("tailscaled.sock"),
        };
        let mut daemon = Self::spawn(&dir, &cli.socket).await?;

        let hostname = format!("--hostname={}", config.tailscale.hostname);
        let auth_key = config
            .tailscale
            .auth_key_file
            .as_ref()
            .map(|file| format!("--auth-key=file:{}", file.path().display()));
        // `--reset` makes the flags the node's complete preferences, so state
        // written by older versions or other tools never blocks `up`.
        let mut up = vec!["up", "--reset", &hostname];
        up.extend(auth_key.as_deref());
        tokio::select! {
            exit = daemon.wait() => bail!("tailscaled exited during login: {}", exit?),
            result = cli.run(&up) => result.context("failed to log the tailnet node in")?,
        };

        let status = cli.status().await?;
        let dns_name = status
            .node
            .map(|node| node.dns_name.trim_end_matches('.').to_owned())
            .filter(|name| !name.is_empty())
            .context("logged-in tailnet node reported no DNS name")?;
        // Issue the certificate now, so the first visitor does not wait for it.
        let expires_at_ms = cli.certificate_expiry_ms(&dns_name).await?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .context("failed to bind the tailnet forwarding listener")?;
        let address = listener.local_addr()?;
        let target = format!("tcp://{address}");
        cli.run(&[
            "serve",
            "--bg",
            &format!("--tls-terminated-tcp={HTTPS_PORT}"),
            "--proxy-protocol=2",
            &target,
        ])
        .await
        .context("failed to forward the tailnet node's HTTPS port")?;

        let url = format!("https://{dns_name}");
        let public_url_matches = match &config.auth.public_url {
            None => {
                config.auth.public_url = Some(url.clone());
                true
            }
            Some(public_url) => crate::auth::Auth::validate_origin(public_url)
                .is_ok_and(|origin| origin.origin().ascii_serialization() == url),
        };
        if !public_url_matches {
            tracing::warn!(public_url = config.public_url(), node = %url,
                "auth.public_url is not the tailnet node's origin; passkeys only work on auth.public_url");
        }
        tracing::info!(%url, "tailnet node is serving HTTPS");
        let observation = Observation {
            state: status.backend_state,
            certificate_error: false,
            expires_at_ms,
        };
        Ok(Some(Self {
            daemon,
            monitor: Monitor {
                status: watch::Sender::new(observation.report(
                    &dns_name,
                    public_url_matches,
                    crate::store::now_ms(),
                )),
                cli,
                dns_name,
                public_url_matches,
                expires_at_ms,
            },
            listener,
            address,
        }))
    }

    /// Starts tailscaled with private state and waits for its socket. Its logs
    /// are forwarded at debug level. It shares piqueld's process group, so
    /// terminal interrupts stop both, and is killed when piqueld drops it.
    async fn spawn(dir: &Path, socket: &Path) -> Result<Child> {
        // A stale socket from a killed daemon would look ready before it is.
        match std::fs::remove_file(socket) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error).context("failed to remove the stale tailscaled socket");
            }
            _ => {}
        }
        let mut daemon = Command::new("tailscaled")
            .arg("--tun=userspace-networking")
            .arg("--port=0")
            .arg("--no-logs-no-support")
            .arg(format!("--statedir={}", dir.display()))
            .arg(format!("--socket={}", socket.display()))
            // Keeps tailscaled's log configuration out of the home directory.
            .env("TS_LOGS_DIR", dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("failed to start tailscaled")?;
        let mut logs =
            BufReader::new(daemon.stderr.take().context("capture tailscaled logs")?).lines();
        tokio::spawn(async move {
            while let Ok(Some(line)) = logs.next_line().await {
                tracing::debug!("tailscaled: {line}");
            }
        });
        let started = Instant::now();
        while !socket.exists() {
            if let Some(exit) = daemon.try_wait()? {
                bail!("tailscaled exited during startup ({exit}); its logs are at debug level");
            }
            ensure!(
                started.elapsed() < STARTUP_TIMEOUT,
                "tailscaled did not open {} within {STARTUP_TIMEOUT:?}",
                socket.display()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(daemon)
    }

    /// Subscribes to the node's periodically refreshed status.
    #[must_use]
    pub fn status(&self) -> watch::Receiver<TailnetStatus> {
        self.monitor.status.subscribe()
    }

    /// Identifies peers connecting through this node.
    #[must_use]
    pub fn whois(&self) -> std::sync::Arc<super::Whois> {
        std::sync::Arc::new(super::Whois::new(self.monitor.cli.clone()))
    }

    /// The node's `MagicDNS` name, without the trailing dot.
    #[must_use]
    pub fn dns_name(&self) -> &str {
        &self.monitor.dns_name
    }

    /// Starts accepting forwarded connections and refreshing status until
    /// cancellation. Returns the listener and the supervisor task, which
    /// cancels the daemon and fails if tailscaled exits, so piqueld exits with
    /// an error and the service manager restarts both.
    #[must_use]
    pub fn listener(
        self,
        cancellation: CancellationToken,
    ) -> (NodeListener, JoinHandle<Result<()>>) {
        let listener = ProxyListener::spawn(self.listener, self.address, cancellation.clone());
        let supervisor = tokio::spawn(self.monitor.run(self.daemon, cancellation));
        let tap: fn(&mut TcpStream) = |_| {};
        (listener.tap_io(tap), supervisor)
    }
}

/// Supervises tailscaled and keeps the reported status current.
struct Monitor {
    cli: Cli,
    dns_name: String,
    public_url_matches: bool,
    expires_at_ms: i64,
    status: watch::Sender<TailnetStatus>,
}

impl Monitor {
    /// Owns tailscaled until cancellation, then drops (and so kills) it.
    /// Fails if tailscaled exits on its own.
    async fn run(mut self, daemon: Child, cancellation: CancellationToken) -> Result<()> {
        Self::supervise(daemon, cancellation, self.refresh_loop()).await
    }

    /// Keep supervision active even while a refresh waits for the CLI.
    async fn supervise(
        mut daemon: Child,
        cancellation: CancellationToken,
        refresh: impl Future<Output = ()>,
    ) -> Result<()> {
        tokio::select! {
            () = cancellation.cancelled() => Ok(()),
            exit = daemon.wait() => {
                // Shutdown signals reach tailscaled too, and may win the race.
                if tokio::time::timeout(SHUTDOWN_RACE, cancellation.cancelled()).await.is_ok() {
                    return Ok(());
                }
                tracing::error!(?exit, "tailscaled exited; stopping piqueld");
                cancellation.cancel();
                bail!("tailscaled exited unexpectedly ({})", exit?);
            }
            () = refresh => Ok(()),
        }
    }

    async fn refresh_loop(&mut self) {
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + REFRESH_INTERVAL,
            REFRESH_INTERVAL,
        );
        loop {
            ticker.tick().await;
            self.refresh().await;
        }
    }

    async fn refresh(&mut self) {
        let state = match self.cli.status().await {
            Ok(status) => status.backend_state,
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "tailnet status refresh failed");
                "Unknown".into()
            }
        };
        let certificate_error = match self.cli.certificate_expiry_ms(&self.dns_name).await {
            Ok(expires_at_ms) => {
                self.expires_at_ms = expires_at_ms;
                false
            }
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "tailnet certificate refresh failed");
                true
            }
        };
        let observation = Observation {
            state,
            certificate_error,
            expires_at_ms: self.expires_at_ms,
        };
        self.status.send_replace(observation.report(
            &self.dns_name,
            self.public_url_matches,
            crate::store::now_ms(),
        ));
    }
}

/// One refresh of the node's login state and certificate.
struct Observation {
    state: String,
    certificate_error: bool,
    expires_at_ms: i64,
}

impl Observation {
    fn report(self, dns_name: &str, public_url_matches: bool, now_ms: i64) -> TailnetStatus {
        let mut problems = Vec::new();
        if self.state != "Running" {
            problems.push(format!("Tailscale is {}", self.state));
        }
        if self.expires_at_ms <= now_ms {
            problems.push("the HTTPS certificate has expired".into());
        }
        if self.certificate_error {
            problems.push("certificate refresh failed; see daemon logs".into());
        }
        let healthy = problems.is_empty();
        if !public_url_matches {
            problems.push(format!(
                "auth.public_url is not https://{dns_name}, so passkeys do not work on this origin"
            ));
        }
        TailnetStatus {
            enabled: true,
            healthy,
            state: self.state,
            dns_name: Some(dns_name.to_owned()),
            certificate_expires_at_ms: Some(self.expires_at_ms),
            public_url_matches,
            message: if problems.is_empty() {
                format!("Serving HTTPS as {dns_name}")
            } else {
                problems.join("; ")
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stalled_refresh_does_not_block_shutdown_or_child_exit() {
        for child_exits in [false, true] {
            // EOF lets the test end the child without signalling other processes.
            let mut daemon = Command::new("sh")
                .args(["-c", "read ignored"])
                .stdin(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let stdin = daemon.stdin.take().unwrap();
            let cancellation = CancellationToken::new();
            let refresh_dropped = CancellationToken::new();
            let guard = refresh_dropped.clone().drop_guard();
            let (started, running) = tokio::sync::oneshot::channel();
            let supervisor = tokio::spawn(Monitor::supervise(
                daemon,
                cancellation.clone(),
                async move {
                    let _guard = guard;
                    started.send(()).unwrap();
                    std::future::pending::<()>().await;
                },
            ));
            running.await.unwrap();
            if child_exits {
                drop(stdin);
            } else {
                cancellation.cancel();
            }
            let result = tokio::time::timeout(Duration::from_secs(5), supervisor)
                .await
                .expect("a stalled refresh must not block supervision")
                .unwrap();
            if child_exits {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("tailscaled exited unexpectedly")
                );
            } else {
                result.unwrap();
            }
            assert!(cancellation.is_cancelled());
            assert!(refresh_dropped.is_cancelled());
        }
    }

    fn observe(state: &str, certificate_error: bool, expires_at_ms: i64) -> Observation {
        Observation {
            state: state.into(),
            certificate_error,
            expires_at_ms,
        }
    }

    #[test]
    fn running_node_with_valid_certificate_is_healthy() {
        let status = observe("Running", false, 2_000).report("piqueld.tail.ts.net", true, 1_000);
        assert!(status.enabled && status.healthy);
        assert_eq!(status.message, "Serving HTTPS as piqueld.tail.ts.net");
        assert_eq!(status.certificate_expires_at_ms, Some(2_000));
    }

    #[test]
    fn login_and_certificate_problems_are_unhealthy() {
        for (observation, expected) in [
            (
                observe("NeedsLogin", false, 2_000),
                "Tailscale is NeedsLogin",
            ),
            (observe("Running", false, 1_000), "certificate has expired"),
            (observe("Running", true, 2_000), "refresh failed"),
        ] {
            let status = observation.report("piqueld.tail.ts.net", true, 1_000);
            assert!(!status.healthy);
            assert!(status.message.contains(expected), "{}", status.message);
        }
    }

    #[test]
    fn public_url_mismatch_is_reported_without_failing_the_node() {
        let status = observe("Running", false, 2_000).report("piqueld.tail.ts.net", false, 1_000);
        assert!(status.healthy);
        assert!(!status.public_url_matches);
        assert!(status.message.contains("https://piqueld.tail.ts.net"));
    }
}
