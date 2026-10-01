//! Go tsnet embedded through libtailscale. A dedicated thread owns the node and
//! its port 443 listener; everything else talks to the node's `LocalAPI`.

use crate::config::DaemonConfig;
use anyhow::{Context, Result, anyhow, ensure};
use axum::serve::{Listener, ListenerExt, TapIo};
use piqueld_core::api::TailnetStatus;
use serde::Deserialize;
use std::{
    net::{IpAddr, SocketAddr},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::fs::DirBuilderExt,
    },
    path::PathBuf,
    sync::{Arc, PoisonError, RwLock},
    time::Duration,
};
use tokio::{
    net::UnixStream,
    sync::{mpsc, oneshot, watch},
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        self,
        crypto::CryptoProvider,
        pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
        server::{ClientHello, ResolvesServerCert},
        sign::CertifiedKey,
    },
    server::TlsStream,
};
use tokio_util::sync::CancellationToken;

const HTTPS_PORT: u16 = 443;
/// Tailscale renews certificates itself; refreshing hands the renewed one to
/// rustls and keeps the reported login state current.
const REFRESH_INTERVAL: Duration = Duration::from_mins(1);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Accepted connections queued between the node thread and the TLS handshakes.
const QUEUE: usize = 64;

type Connection = (std::os::unix::net::UnixStream, IpAddr);

/// A logged-in tailnet node holding its HTTPS certificate.
pub struct Node {
    monitor: Monitor,
    acceptor: TlsAcceptor,
    connections: mpsc::Receiver<Connection>,
    address: SocketAddr,
}

/// The node's HTTPS listener. The no-op `tap_io` wrapper is what lets axum
/// supply `ConnectInfo<SocketAddr>`, which authentication throttling needs;
/// axum otherwise derives it only for TCP listeners.
pub type NodeListener = TapIo<TlsListener, fn(&mut TlsStream<UnixStream>)>;

impl Node {
    /// Starts the node when `tailscale.enabled` is set and waits until it is
    /// logged in and holds a certificate. An unset `auth.public_url` becomes
    /// the node's HTTPS origin.
    ///
    /// Without an auth key or saved state, the node needs an interactive
    /// login; the login URL is logged and startup waits for it.
    ///
    /// # Errors
    /// Returns state directory, auth key, node startup, `LocalAPI`, or
    /// certificate errors. HTTPS certificates must be enabled for the tailnet.
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
        let auth_key = config
            .tailscale
            .auth_key_file
            .as_ref()
            .map(|path| {
                std::fs::read_to_string(path)
                    .map(|key| key.trim().to_owned())
                    .with_context(|| format!("failed to read auth key {}", path.display()))
            })
            .transpose()?;
        let (started, loopback) = oneshot::channel();
        let (sender, connections) = mpsc::channel(QUEUE);
        let thread = Thread {
            dir,
            hostname: config.tailscale.hostname.clone(),
            auth_key,
        };
        // A plain thread, not the blocking pool: accept blocks until the next
        // connection, and runtime shutdown must not wait for it.
        std::thread::Builder::new()
            .name("tailnet".into())
            .spawn(move || thread.run(started, &sender))
            .context("failed to spawn the tailnet thread")?;
        let local_api = LocalApi::new(
            loopback
                .await
                .context("tailnet thread exited during startup")?
                .map_err(|error| anyhow!(error))
                .context("failed to start the tailnet node")?,
        )?;

        let status = local_api.wait_until_running().await?;
        let dns_name = status
            .node
            .map(|node| node.dns_name.trim_end_matches('.').to_owned())
            .context("running tailnet node reported no DNS name")?;
        let address = SocketAddr::new(
            *status
                .addresses
                .unwrap_or_default()
                .first()
                .context("running tailnet node reported no addresses")?,
            HTTPS_PORT,
        );
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let served = local_api.certificate(&dns_name, &provider).await?;
        let expires_at_ms = served.expires_at_ms;
        let certificate = Arc::new(Certificate(RwLock::new(served)));
        let mut tls = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .context("failed to configure TLS")?
            .with_no_client_auth()
            .with_cert_resolver(certificate.clone());
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];

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
        let monitor = Monitor {
            status: watch::Sender::new(observation.report(
                &dns_name,
                public_url_matches,
                crate::store::now_ms(),
            )),
            local_api,
            dns_name,
            public_url_matches,
            provider,
            certificate,
        };
        Ok(Some(Self {
            monitor,
            acceptor: TlsAcceptor::from(Arc::new(tls)),
            connections,
            address,
        }))
    }

    /// Subscribes to the node's periodically refreshed status.
    #[must_use]
    pub fn status(&self) -> watch::Receiver<TailnetStatus> {
        self.monitor.status.subscribe()
    }

    /// The node's `MagicDNS` name, without the trailing dot.
    #[must_use]
    pub fn dns_name(&self) -> &str {
        &self.monitor.dns_name
    }

    /// Starts TLS handshakes and status refreshes until cancellation. If the
    /// node stops accepting connections, the daemon is cancelled.
    #[must_use]
    pub fn listener(self, cancellation: CancellationToken) -> NodeListener {
        let (sender, streams) = mpsc::channel(QUEUE);
        tokio::spawn(self.monitor.run(cancellation.clone()));
        tokio::spawn(Self::handshake(
            self.connections,
            self.acceptor,
            sender,
            cancellation,
        ));
        let tap: fn(&mut TlsStream<UnixStream>) = |_| {};
        TlsListener {
            streams,
            address: self.address,
        }
        .tap_io(tap)
    }

    async fn handshake(
        mut connections: mpsc::Receiver<Connection>,
        acceptor: TlsAcceptor,
        streams: mpsc::Sender<(TlsStream<UnixStream>, SocketAddr)>,
        cancellation: CancellationToken,
    ) {
        loop {
            let (stream, peer) = tokio::select! {
                () = cancellation.cancelled() => return,
                connection = connections.recv() => {
                    if let Some(connection) = connection { connection } else {
                        tracing::error!("tailnet node stopped accepting connections; stopping piqueld");
                        cancellation.cancel();
                        return;
                    }
                }
            };
            let (acceptor, streams) = (acceptor.clone(), streams.clone());
            // Handshakes run concurrently so one slow client cannot block others.
            tokio::spawn(async move {
                let accepted = async {
                    stream.set_nonblocking(true)?;
                    let stream = UnixStream::from_std(stream)?;
                    tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream))
                        .await
                        .map_err(std::io::Error::from)?
                };
                match accepted.await {
                    Ok(stream) => {
                        // A closed queue means the server is shutting down.
                        let _ = streams.send((stream, SocketAddr::new(peer, 0))).await;
                    }
                    Err(error) => tracing::debug!(%peer, %error, "tailnet TLS handshake failed"),
                }
            });
        }
    }
}

/// Owns the libtailscale handle, which borrows into its listener, for the
/// process lifetime.
struct Thread {
    dir: PathBuf,
    hostname: String,
    auth_key: Option<String>,
}

impl Thread {
    fn run(
        self,
        started: oneshot::Sender<Result<libtailscale::Loopback, String>>,
        connections: &mpsc::Sender<Connection>,
    ) {
        let mut node = libtailscale::Tailscale::new();
        let listener = match self
            .start(&mut node)
            .and_then(|loopback| Ok((loopback, node.listen("tcp", &format!(":{HTTPS_PORT}"))?)))
        {
            Ok((loopback, listener)) => {
                if started.send(Ok(loopback)).is_err() {
                    return;
                }
                listener
            }
            Err(error) => {
                let _ = started.send(Err(error));
                return;
            }
        };
        loop {
            let stream = match listener.accept() {
                Ok(stream) => stream,
                Err(error) => {
                    tracing::error!(%error, "tailnet listener failed");
                    return;
                }
            };
            let peer = match listener.get_remote_addr(stream.as_raw_fd()) {
                Ok(peer) => peer,
                Err(error) => {
                    tracing::warn!(%error, "dropping tailnet connection without a peer address");
                    continue;
                }
            };
            // libtailscale hands out one end of a Unix socket pair.
            let stream = std::os::unix::net::UnixStream::from(OwnedFd::from(stream));
            if connections.blocking_send((stream, peer)).is_err() {
                return;
            }
        }
    }

    fn start(&self, node: &mut libtailscale::Tailscale) -> Result<libtailscale::Loopback, String> {
        node.set_dir(
            self.dir
                .to_str()
                .ok_or("tailnet state path must be UTF-8")?,
        )?;
        node.set_hostname(&self.hostname)?;
        if let Some(key) = &self.auth_key {
            node.set_authkey(key)?;
        }
        node.start()?;
        node.loopback()
    }
}

/// Keeps the served certificate and reported status current.
struct Monitor {
    local_api: LocalApi,
    dns_name: String,
    public_url_matches: bool,
    provider: Arc<CryptoProvider>,
    certificate: Arc<Certificate>,
    status: watch::Sender<TailnetStatus>,
}

impl Monitor {
    async fn run(self, cancellation: CancellationToken) {
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + REFRESH_INTERVAL,
            REFRESH_INTERVAL,
        );
        loop {
            tokio::select! {
                () = cancellation.cancelled() => return,
                _ = ticker.tick() => self.refresh().await,
            }
        }
    }

    async fn refresh(&self) {
        let state = match self.local_api.status().await {
            Ok(status) => status.backend_state,
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "tailnet status refresh failed");
                "Unknown".into()
            }
        };
        let certificate_error = match self
            .local_api
            .certificate(&self.dns_name, &self.provider)
            .await
        {
            Ok(served) => {
                *self
                    .certificate
                    .0
                    .write()
                    .unwrap_or_else(PoisonError::into_inner) = served;
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
            expires_at_ms: self.certificate.expires_at_ms(),
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

/// The certificate currently handed to TLS handshakes.
#[derive(Debug)]
struct Certificate(RwLock<Served>);

#[derive(Debug)]
struct Served {
    key: Arc<CertifiedKey>,
    expires_at_ms: i64,
}

impl Certificate {
    fn expires_at_ms(&self) -> i64 {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .expires_at_ms
    }
}

impl ResolvesServerCert for Certificate {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(Arc::clone(
            &self.0.read().unwrap_or_else(PoisonError::into_inner).key,
        ))
    }
}

/// The node's `LocalAPI`, served on loopback with a per-process credential.
struct LocalApi {
    client: reqwest::Client,
    address: String,
    credential: String,
}

/// The fields piqueld reads from `ipnstate.Status`.
#[derive(Deserialize)]
struct Status {
    #[serde(rename = "BackendState")]
    backend_state: String,
    #[serde(rename = "AuthURL", default)]
    auth_url: String,
    #[serde(rename = "TailscaleIPs", default)]
    addresses: Option<Vec<IpAddr>>,
    #[serde(rename = "Self")]
    node: Option<NodeStatus>,
}

#[derive(Deserialize)]
struct NodeStatus {
    #[serde(rename = "DNSName")]
    dns_name: String,
}

impl LocalApi {
    fn new(loopback: libtailscale::Loopback) -> Result<Self> {
        Ok(Self {
            // Issuing a certificate can take a while: Let's Encrypt validates
            // a DNS challenge.
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_mins(2))
                .build()
                .context("failed to initialize the LocalAPI client")?,
            address: loopback.address,
            credential: loopback.credential,
        })
    }

    async fn get(&self, path: &str) -> Result<Vec<u8>> {
        let response = self
            .client
            .get(format!("http://{}/localapi/v0/{path}", self.address))
            .basic_auth("", Some(&self.credential))
            .header("Sec-Tailscale", "localapi")
            .send()
            .await
            .with_context(|| format!("LocalAPI {path} request failed"))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .with_context(|| format!("LocalAPI {path} response failed"))?;
        ensure!(
            status.is_success(),
            "LocalAPI {path} returned {status}: {}",
            String::from_utf8_lossy(&body).trim()
        );
        Ok(body.to_vec())
    }

    async fn status(&self) -> Result<Status> {
        serde_json::from_slice(&self.get("status").await?).context("invalid LocalAPI status")
    }

    /// Polls until the node is logged in, logging state changes and login URLs.
    async fn wait_until_running(&self) -> Result<Status> {
        let (mut state, mut login) = (String::new(), String::new());
        loop {
            let status = self.status().await?;
            if status.backend_state == "Running"
                && status
                    .node
                    .as_ref()
                    .is_some_and(|node| !node.dns_name.is_empty())
            {
                return Ok(status);
            }
            if status.backend_state != state {
                tracing::info!(state = %status.backend_state, "waiting for the tailnet node to log in");
                state.clone_from(&status.backend_state);
            }
            if !status.auth_url.is_empty() && status.auth_url != login {
                tracing::warn!(url = %status.auth_url,
                    "log the tailnet node in at this URL, or set tailscale.auth_key_file");
                login = status.auth_url;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// Fetches the certificate for the node's name. Tailscale caches it in the
    /// node state and renews it before expiry.
    async fn certificate(&self, dns_name: &str, provider: &CryptoProvider) -> Result<Served> {
        let pem = self
            .get(&format!("cert/{dns_name}?type=pair"))
            .await
            .context("failed to obtain the tailnet HTTPS certificate; enable HTTPS certificates for the tailnet")?;
        let key = PrivateKeyDer::from_pem_slice(&pem).context("certificate has no private key")?;
        let chain = CertificateDer::pem_slice_iter(&pem)
            .collect::<Result<Vec<_>, _>>()
            .context("invalid certificate PEM")?;
        let (_, leaf) =
            x509_parser::parse_x509_certificate(chain.first().context("certificate is empty")?)
                .map_err(|error| anyhow!("invalid certificate: {error}"))?;
        let expires_at_ms = leaf.validity().not_after.timestamp().saturating_mul(1000);
        Ok(Served {
            key: Arc::new(
                CertifiedKey::from_der(chain, key, provider)
                    .context("certificate does not match its key")?,
            ),
            expires_at_ms,
        })
    }
}

/// Accepted, TLS-terminated connections from the node.
pub struct TlsListener {
    streams: mpsc::Receiver<(TlsStream<UnixStream>, SocketAddr)>,
    address: SocketAddr,
}

impl Listener for TlsListener {
    type Io = TlsStream<UnixStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.streams.recv().await {
            Some(connection) => connection,
            // The handshake task only stops on shutdown, which also stops serving.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.address)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn status_reads_login_state_and_node_name() {
        let status: Status = serde_json::from_str(
            r#"{"BackendState":"Running","AuthURL":"","TailscaleIPs":["100.64.0.1"],
                "Self":{"DNSName":"piqueld.tail.ts.net."}}"#,
        )
        .unwrap();
        assert_eq!(status.node.unwrap().dns_name, "piqueld.tail.ts.net.");
        let login: Status = serde_json::from_str(
            r#"{"BackendState":"NeedsLogin","AuthURL":"https://login.tailscale.com/a/1",
                "TailscaleIPs":null,"Self":{"DNSName":""}}"#,
        )
        .unwrap();
        assert_eq!(login.auth_url, "https://login.tailscale.com/a/1");
        assert!(login.addresses.is_none());
    }
}
