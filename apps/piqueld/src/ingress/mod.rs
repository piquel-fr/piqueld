//! Installation-owned Caddy gateway, independent of the daemon's process
//! lifetime. Public routes are served on its published listener, or on a
//! tunnel listener that only `cloudflared` reaches; private routes on a
//! private listener that only the apps tailnet node reaches.
mod certificates;
mod configuration;
mod gateway;
mod node;
mod records;
mod tunnel;
mod wire;

use crate::{
    config::{AcmeConfig, DnsConfig, PrivateIngressConfig, TunnelConfig, TunnelCredentials},
    dns::Dns,
    store::{Store, ingress::RoutingTable, now_ms},
};
use anyhow::{Context, Result};
use certificates::Certificates;
use futures_util::{StreamExt, stream};
use piqueld_core::{
    EnvironmentId,
    api::{DnsRecords, IngressStatus, PrivateIngressStatus, PublicIngressStatus, RouteStatus},
    manifest::{Hostname, RouteHostname, ValidatedRoute, Visibility},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::IpAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
pub use wire::ResponseError;
use wire::UnixApi;

/// Version released with piqueld; upgrades deliberately replace the gateway.
pub const CADDY_IMAGE: &str =
    "caddy:2.11.4-alpine@sha256:6aeddd44c3078b0f9a35206472a11420648a79c184603ef95957d0a20044cb2b";
/// Tailnet client address private probes present, Tailscale's own service address.
const PROBE_CLIENT: std::net::SocketAddrV4 =
    std::net::SocketAddrV4::new(std::net::Ipv4Addr::new(100, 100, 100, 100), 0);
/// Version of the apps tailnet node released with piqueld; upgrades replace it.
pub const TAILSCALE_IMAGE: &str = "tailscale/tailscale:v1.102.5@sha256:c507f3a2a6ab1cabd8d809b98edeb41edbd5c3fb6ad9632ffd098b4c7d0b4065";
/// Version of `cloudflared` released with piqueld; upgrades replace it.
pub const CLOUDFLARED_IMAGE: &str = "cloudflare/cloudflared:2026.10.0@sha256:9b49eed8f62806d5d45ddf59ecefb5710429598ea6d3fcccd2af938f621b2b07";

/// Socket in the control mount that the gateway forwards webhook deliveries to.
const WEBHOOK_SOCKET: &str = "webhooks.sock";
/// Socket in the control mount that the gateway forwards the dashboard
/// hostname to.
const DASHBOARD_SOCKET: &str = "dashboard.sock";

/// Ticks of a background loop, every period until cancellation; missed
/// ticks are skipped.
struct Ticks<'a> {
    interval: tokio::time::Interval,
    cancellation: &'a CancellationToken,
}

impl<'a> Ticks<'a> {
    /// Ticks every `period`, the first at once.
    fn every(period: Duration, cancellation: &'a CancellationToken) -> Self {
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        Self {
            interval,
            cancellation,
        }
    }

    /// Waits for the next tick: `false` once cancelled.
    async fn next(&mut self) -> bool {
        tokio::select! {
            () = self.cancellation.cancelled() => false,
            _ = self.interval.tick() => true,
        }
    }
}

/// The daemon's end of the dashboard hostname, which the private listener
/// forwards to it.
pub struct DashboardSocket {
    /// The bound socket in the gateway's control mount.
    pub listener: tokio::net::UnixListener,
    /// `ingress.private.dashboard_hostname`.
    pub hostname: Hostname,
    /// Identifies the tailnet clients the apps node forwards.
    pub whois: Arc<crate::tailnet::Whois>,
}

/// Serializes gateway configuration, lifecycle, and durable route projection.
pub struct Ingress {
    /// Whether the daemon configuration enables ingress; disabled ingress stops
    /// the gateway and withdraws routes.
    pub(crate) enabled: bool,
    /// Store instance identity, served by the probe endpoint to verify DNS.
    instance_id: String,
    /// Gateway container and edge network name, derived from the instance identity.
    name: String,
    /// Host directory holding Caddy's data, config, and control mounts.
    directory: PathBuf,
    /// Raw Docker Engine API over its Unix socket.
    docker: UnixApi,
    /// Caddy's private admin API socket inside the control mount.
    caddy: UnixApi,
    /// Docker client used only for streaming image pulls, which needs
    /// Docker's socket only once it pulls.
    images: crate::docker::BollardDocker,
    /// Public HTTPS client for route probes; never follows redirects or proxies.
    client: reqwest::Client,
    store: Arc<Store>,
    /// Gateway writer lock serializing configuration and lifecycle changes.
    update: Mutex<()>,
    /// Operation whose deployment the current gateway pass serves, set under
    /// `update`; the pass's actions record its actor. `None` for the
    /// periodic repair pass, which the daemon makes on its own.
    requester: Mutex<Option<String>>,
    health: RwLock<IngressStatus>,
    /// Unix timestamp (seconds) up to which each container's logs were relayed.
    logs_since: Mutex<BTreeMap<String, u64>>,
    /// DNS providers and the DNS-01 certificates loaded into Caddy.
    certificates: Certificates,
    /// The apps tailnet node, while `[ingress.private]` is enabled.
    node: Option<node::Node>,
    /// Private hostname the gateway forwards to the daemon's dashboard
    /// socket, `ingress.private.dashboard_hostname`.
    dashboard: Option<Hostname>,
    /// The Cloudflare Tunnel serving public routes instead of ports 80/443,
    /// while `[ingress.tunnel]` is enabled.
    tunnel: Option<TunnelCredentials>,
    /// `[ingress] public_addresses`, which direct public routes' managed
    /// A/AAAA records point at.
    public_addresses: Vec<IpAddr>,
    /// Each route hostname's DNS record state from the latest pass.
    records: parking_lot::RwLock<BTreeMap<RouteHostname, records::RouteDns>>,
    /// Set once the gateway has converged, or stopped, with this daemon's
    /// configuration; DNS records wait for it.
    gateway_converged: std::sync::atomic::AtomicBool,
    /// Set once Swarm's address pools are verified outside the tailnet ranges.
    tailnet_pools: tokio::sync::OnceCell<()>,
    /// Public hostname whose webhook path the gateway forwards to the
    /// daemon's webhook socket, `ingress.webhook_hostname`.
    webhooks: Option<Hostname>,
    #[cfg(test)]
    issuer: Option<serde_json::Value>,
    #[cfg(test)]
    extra_hosts: Vec<String>,
    /// Publishes the private listener on the engine's port 8443, trusts PROXY
    /// headers from Docker bridges, and probes it on this loopback port, so
    /// tests outside the engine can reach it.
    #[cfg(test)]
    private_port: Option<u16>,
}

impl Ingress {
    /// Creates an inert manager. Startup failures are reported by its background loop.
    ///
    /// ```text
    /// name: piqueld-ingress-<first 16 hex chars of sha256(instance_id)>
    /// directory: <data_dir>/ingress
    /// ```
    /// # Errors
    /// Returns invalid local client configuration errors.
    pub fn new(enabled: bool, socket: &Path, data_dir: &Path, store: Arc<Store>) -> Result<Self> {
        use sha2::{Digest, Sha256};
        let suffix = format!(
            "{:x}",
            Sha256::digest(store.instance_id().as_str().as_bytes())
        );
        let directory = data_dir.join("ingress");
        // Local Docker and Caddy control requests share the daemon's request budget.
        // Deployment convergence still imposes its configured outer deadline.
        let request_timeout = crate::docker::DockerTimeout::Request.duration();
        let certificates = Certificates::new(
            Dns::new(Vec::new())?,
            AcmeConfig::default(),
            directory.clone(),
        );
        Ok(Self {
            enabled,
            instance_id: store.instance_id().to_string(),
            name: format!("piqueld-ingress-{}", &suffix[..16]),
            caddy: UnixApi::new(directory.join("control/admin.sock"), request_timeout),
            directory,
            docker: UnixApi::new(socket.into(), request_timeout),
            images: crate::docker::BollardDocker::connect(socket)
                .context("Docker socket is not UTF-8")?,
            client: Self::probe_client().build()?,
            store,
            update: Mutex::new(()),
            requester: Mutex::new(None),
            health: RwLock::new(IngressStatus {
                enabled,
                healthy: false,
                gateway: false,
                message: "Waiting for gateway reconciliation".into(),
                public: PublicIngressStatus::default(),
                private: PrivateIngressStatus::default(),
                routes: Vec::new(),
            }),
            logs_since: Mutex::default(),
            certificates,
            node: None,
            dashboard: None,
            tunnel: None,
            public_addresses: Vec::new(),
            records: parking_lot::RwLock::default(),
            gateway_converged: std::sync::atomic::AtomicBool::new(false),
            tailnet_pools: tokio::sync::OnceCell::new(),
            webhooks: None,
            #[cfg(test)]
            issuer: None,
            #[cfg(test)]
            extra_hosts: Vec::new(),
            #[cfg(test)]
            private_port: None,
        })
    }

    /// The ingress `config` describes: its listeners, DNS providers and ACME
    /// account.
    ///
    /// # Errors
    /// Returns invalid local client configuration errors.
    pub fn from_config(config: &crate::config::DaemonConfig, store: Arc<Store>) -> Result<Self> {
        Ok(Self::new(
            config.ingress.enabled,
            &config.docker.socket,
            &config.server.data_dir,
            store,
        )?
        .with_dns(&config.dns, &config.ingress.acme)?
        .with_private(&config.ingress.private)
        .with_tunnel(&config.ingress.tunnel)
        .with_public_addresses(&config.ingress.public_addresses)
        .with_webhooks(config.ingress.webhook_hostname.clone()))
    }

    /// Points direct public routes' managed A/AAAA records at `addresses`.
    #[must_use]
    pub fn with_public_addresses(mut self, addresses: &[IpAddr]) -> Self {
        self.public_addresses = addresses.to_vec();
        self
    }

    /// Forwards GitHub webhook deliveries on `hostname` to the daemon.
    #[must_use]
    pub fn with_webhooks(mut self, hostname: Option<Hostname>) -> Self {
        self.webhooks = hostname;
        self
    }

    /// The public hostname the gateway forwards webhook deliveries on, while
    /// ingress is enabled.
    #[must_use]
    pub fn webhook_hostname(&self) -> Option<&Hostname> {
        self.webhooks.as_ref().filter(|_| self.enabled)
    }

    /// Binds the socket webhook deliveries arrive on whenever a webhook
    /// hostname is configured: the gateway forwards to it while ingress is
    /// enabled, and otherwise another proxy running as the daemon's user may.
    ///
    /// # Errors
    /// Returns directory preparation or binding errors.
    pub async fn bind_webhooks(&self) -> Result<Option<tokio::net::UnixListener>> {
        if self.webhooks.is_none() {
            return Ok(None);
        }
        Self::bind_control(&self.directory, WEBHOOK_SOCKET)
            .await
            .map(Some)
    }

    /// Binds the socket the private listener forwards the dashboard hostname
    /// to, when `config` sets one. Called before the daemon opens its state,
    /// like its other listeners.
    ///
    /// # Errors
    /// Returns directory preparation or binding errors.
    pub async fn bind_dashboard(
        config: &crate::config::DaemonConfig,
    ) -> Result<Option<DashboardSocket>> {
        let Some(hostname) = config.ingress.private.dashboard_hostname.clone() else {
            return Ok(None);
        };
        let directory = config.server.data_dir.join("ingress");
        Ok(Some(DashboardSocket {
            listener: Self::bind_control(&directory, DASHBOARD_SOCKET).await?,
            hostname,
            whois: Arc::new(crate::tailnet::Whois::at(node::Node::socket(&directory))),
        }))
    }

    /// Binds the daemon socket `name` in the gateway's control mount, below
    /// the ingress `directory`. Only the daemon's user, which the gateway
    /// runs as, may connect. A socket left by an earlier run is replaced: the
    /// data directory lock is held.
    async fn bind_control(directory: &Path, name: &str) -> Result<tokio::net::UnixListener> {
        let control = directory.join("control");
        for directory in [directory, &control] {
            crate::prepare_data_dir(directory).await?;
        }
        let path = control.join(name);
        crate::directories::remove_file(&path)
            .await
            .with_context(|| format!("remove the previous {name}"))?;
        let previous = rustix::process::umask(rustix::fs::Mode::RWXG | rustix::fs::Mode::RWXO);
        let bound = tokio::net::UnixListener::bind(&path);
        rustix::process::umask(previous);
        let listener = bound.with_context(|| format!("bind {name}"))?;
        tracing::info!(socket = %path.display(), "control socket bound");
        Ok(listener)
    }

    /// The private hostname the gateway forwards to the daemon's dashboard
    /// socket. Configuration only allows one with private ingress enabled.
    #[must_use]
    pub fn dashboard_hostname(&self) -> Option<&Hostname> {
        self.dashboard.as_ref().filter(|_| self.enabled)
    }

    /// Serves private routes through the apps tailnet node when
    /// `[ingress.private]` enables it.
    #[must_use]
    pub fn with_private(mut self, config: &PrivateIngressConfig) -> Self {
        self.node = node::Node::new(config, &self.directory);
        self.dashboard = config
            .dashboard_hostname
            .clone()
            .filter(|_| self.node.is_some());
        self
    }

    /// Serves public routes through a Cloudflare Tunnel, instead of ports
    /// 80/443, when `[ingress.tunnel]` enables it.
    #[must_use]
    pub fn with_tunnel(mut self, config: &TunnelConfig) -> Self {
        self.tunnel.clone_from(&config.credentials);
        self
    }

    /// HTTPS client settings for route probes: bounded, and never following
    /// redirects or proxies.
    fn probe_client() -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
    }

    /// Configures DNS providers, for DNS-01 certificates and route records,
    /// and the ACME account.
    ///
    /// # Errors
    /// Returns HTTP client initialization failures.
    pub fn with_dns(mut self, dns: &DnsConfig, acme: &AcmeConfig) -> Result<Self> {
        self.certificates = Certificates::new(
            Dns::new(dns.providers.clone())?,
            acme.clone(),
            self.directory.clone(),
        );
        Ok(self)
    }

    /// Latest bounded background health snapshot; this never waits for Docker.
    pub async fn status(&self) -> IngressStatus {
        self.health.read().await.clone()
    }

    /// Holds the gateway writer lock, as a slow update for another application does.
    #[cfg(test)]
    pub(crate) async fn hold_updates(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.update.lock().await
    }

    /// Applies one application's deployment boundary under the gateway writer lock.
    /// Stages the routes, then synchronizes the gateway unless backends are not
    /// ready yet and no route is being withdrawn. A route whose visibility
    /// changes is withdrawn from its old listener like a removal. Fails for
    /// this application's own network failures, not for other applications'.
    pub(crate) async fn apply(
        &self,
        operation: &piqueld_core::Operation,
        routes: &[ValidatedRoute],
        ready: bool,
    ) -> Result<()> {
        let _guard = self.update.lock().await;
        *self.requester.lock().await = Some(operation.id.clone());
        let id = &operation.environment_id;
        let previous = self.store.applied_routes(id).await?;
        self.store
            .stage_routes(id, routes, ready, Some(&operation.id))
            .await?;
        // Provisioning must proceed before newly enabled ingress networks exist.
        // Only actual withdrawals require gateway I/O before backend readiness.
        if !ready
            && previous
                .iter()
                .all(|old| routes.iter().any(|new| new.same_listener(old)))
        {
            return Ok(());
        }
        self.synchronize_for(Some(id)).await
    }

    /// Repairs lifecycle/configuration, probes public HTTPS, and maintains
    /// certificates and DNS records, independently of deployments.
    pub async fn run(&self, cancellation: CancellationToken) {
        tokio::join!(
            self.run_gateway(&cancellation),
            self.run_probes(&cancellation),
            self.run_certificates(&cancellation),
            self.run_records(&cancellation)
        );
    }

    /// Hostnames served with DNS-01 certificates instead of Caddy's automatic
    /// HTTPS: private routes and the dashboard hostname, which a public CA
    /// cannot reach, while private ingress is enabled, and public wildcards,
    /// which only DNS-01 can prove, unless Cloudflare terminates their TLS.
    fn dns01_hostnames(&self, table: &RoutingTable) -> BTreeSet<RouteHostname> {
        table
            .values()
            .flatten()
            .filter(|route| match route.visibility {
                Visibility::Private => self.node.is_some(),
                Visibility::Public => self.tunnel.is_none() && route.hostname.wildcard().is_some(),
            })
            .map(|route| route.hostname.clone())
            .chain(self.dashboard_hostname().cloned().map(RouteHostname::from))
            .collect()
    }

    /// Every minute, issues and renews the DNS-01 certificates deployed routes
    /// need. The gateway loop loads new certificates into Caddy.
    async fn run_certificates(&self, cancellation: &CancellationToken) {
        let mut ticks = Ticks::every(Duration::from_mins(1), cancellation);
        while ticks.next().await {
            let desired = if self.enabled {
                match self.store.routing_table().await {
                    Ok(table) => self.dns01_hostnames(&table),
                    Err(error) => {
                        tracing::warn!(error=?error, "could not read routes needing certificates");
                        continue;
                    }
                }
            } else {
                BTreeSet::new()
            };
            // Not dropped on shutdown: an order in progress deletes its TXT
            // record and finishes its journal action first.
            self.maintain_certificates(&desired, now_ms(), cancellation)
                .await;
        }
    }

    /// Every 10s, reconciles the gateway (see [`Self::refresh`]) and relays
    /// Caddy logs. Failures are logged and retried on the next tick.
    async fn run_gateway(&self, cancellation: &CancellationToken) {
        let mut ticks = Ticks::every(Duration::from_secs(10), cancellation);
        while ticks.next().await {
            let pass = async {
                if let Err(error) = self.refresh().await {
                    tracing::warn!(?error, "ingress reconciliation failed; will retry");
                }
                if let Err(error) = self.relay_logs().await {
                    tracing::debug!(?error, "could not read Caddy diagnostics");
                }
            };
            if cancellation.run_until_cancelled(pass).await.is_none() {
                return;
            }
        }
    }

    /// Every 15s, refreshes route status by probing HTTPS for acknowledged routes.
    async fn run_probes(&self, cancellation: &CancellationToken) {
        let mut ticks = Ticks::every(Duration::from_secs(15), cancellation);
        while ticks.next().await {
            // Slow public DNS/TLS must never delay gateway drift repair or disablement.
            let probes = cancellation.run_until_cancelled(self.probe_routes());
            if probes.await.is_none() {
                return;
            }
        }
    }

    /// Reconciles the gateway for all applications under the writer lock,
    /// as its periodic pass does, e.g. to withdraw a stopped environment's
    /// routes before the stop is answered.
    pub(crate) async fn refresh(&self) -> Result<()> {
        let _guard = self.update.lock().await;
        *self.requester.lock().await = None;
        self.synchronize().await
    }

    /// Reconciles the gateway for all applications.
    async fn synchronize(&self) -> Result<()> {
        self.synchronize_for(None).await
    }

    /// Converges the gateway with the durable routing table and updates health.
    ///
    /// When enabled: verifies ingress networks, ensures the gateway container,
    /// applies routes and attachments, acknowledges the applied table, then
    /// ensures `cloudflared` and the apps node. When disabled: stops the
    /// gateway, `cloudflared` and the node and acknowledges the withdrawal.
    /// Callers must hold the writer lock. Network failures degrade health;
    /// they fail the call only for `application` (or for any application when
    /// `None`). A disconnected tunnel degrades health without failing the
    /// call. Node failures degrade only the private listener's health, which
    /// public routes do not depend on, but still count towards the
    /// `ingress_unavailable` condition so they notify.
    async fn synchronize_for(
        &self,
        application: Option<&piqueld_core::EnvironmentId>,
    ) -> Result<()> {
        let mut failures = std::collections::BTreeMap::new();
        // Health messages name only the failed stage. Error details, which may
        // include Docker/Caddy response bodies, stay in daemon logs.
        let mut stage = "read deployed routing configuration";
        // Whether the gateway converged (or stopped), even if cloudflared did
        // not: the private listener depends only on the gateway.
        let mut gateway = false;
        let result: Result<()> = async {
            let table = self.store.served_table().await?;
            if self.enabled {
                stage = "verify application ingress networks";
                let (accepted, networks, rejected) = self.prepare_routes(&table).await?;
                failures = rejected;
                stage = if self.tunnel.is_some() {
                    "prepare the Caddy gateway (requires Docker 28+)"
                } else {
                    "prepare the Caddy gateway (requires free ports 80/443 and Docker 28+)"
                };
                self.ensure_gateway(&accepted, &networks).await?;
                stage = "apply Caddy routes and network attachments";
                self.configure_gateway(&accepted, &networks).await?;
                stage = "record applied routes";
                self.store.acknowledge_routes(&accepted).await?;
                gateway = true;
                self.gateway_converged
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                stage = "run cloudflared for the Cloudflare Tunnel";
                self.ensure_tunnel().await?;
            } else {
                stage = "stop the disabled Caddy gateway";
                // Each is attempted even if the other fails.
                let stopped = self.stop_gateway().await;
                let removed = self.remove_tunnel_credentials().await;
                stopped?;
                stage = "remove the Cloudflare Tunnel credentials";
                removed?;
                stage = "record withdrawn routes";
                self.store.acknowledge_routes(&table).await?;
                gateway = true;
                self.gateway_converged
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            anyhow::Ok(())
        }
        .await
        .context(stage);
        let private = self.private_status(gateway).await;
        let public = self.public_status().await;
        // In tunnel mode, public routes are unreachable while it is down.
        let disconnected = self.enabled
            && matches!(
                public,
                PublicIngressStatus::Tunnel {
                    connected: false,
                    ..
                }
            );
        let healthy = result.is_ok() && failures.is_empty() && !disconnected;
        self.observe_availability(healthy, &private).await;
        let mut health = self.health.write().await;
        health.healthy = healthy;
        health.gateway = gateway;
        health.public = public;
        health.private = private;
        health.message = match &result {
            Ok(()) if !failures.is_empty() => format!(
                "Ingress degraded: {} application network(s) unavailable. See daemon logs for details.",
                failures.len()
            ),
            Ok(()) if disconnected => "Caddy is running, but the Cloudflare Tunnel is not connected, so public routes are unreachable. See the tunnel's status".into(),
            Ok(()) if self.enabled => {
                "Caddy is running and routing configuration is applied".into()
            }
            Ok(()) => "Ingress is disabled in daemon TOML; its listeners are stopped".into(),
            Err(error) => {
                tracing::error!(error=?error, "managed ingress is unhealthy");
                format!("Ingress unavailable: could not {stage}. See daemon logs for details.")
            }
        };
        result?;
        for (id, error) in failures {
            if application.is_none_or(|target| target == &id) {
                anyhow::bail!("application {id} ingress network: {error}");
            }
        }
        Ok(())
    }

    /// Records whether ingress is available: the public side is `healthy` and
    /// the `private` listener too. Transitions become history, and sustained
    /// failures notify administrators, like other daemon dependencies.
    async fn observe_availability(&self, healthy: bool, private: &PrivateIngressStatus) {
        let failed = !(healthy && private.healthy);
        if let Err(error) = self
            .store
            .observe_condition("ingress_unavailable", None, failed, now_ms(), 45_000)
            .await
        {
            tracing::error!(error=?error, "ingress health observation could not be recorded");
        }
    }

    /// The private listener's health once the gateway is up: the apps node's
    /// state. Ensures the node. While private ingress is disabled, removes a
    /// leftover node even when the gateway fails, and is healthy once none
    /// runs. Callers hold the writer lock.
    async fn private_status(&self, gateway: bool) -> PrivateIngressStatus {
        let status = |healthy: bool, message: &str| PrivateIngressStatus {
            enabled: self.node.is_some(),
            healthy,
            message: message.into(),
            ..PrivateIngressStatus::default()
        };
        // Disabled ingress stops the node with the gateway.
        if !self.enabled {
            return if gateway {
                status(
                    true,
                    "Ingress is disabled in daemon TOML; the apps node is stopped",
                )
            } else {
                status(
                    false,
                    "Stopping the apps node is not confirmed; private routes may still be reachable from the tailnet. See ingress health",
                )
            };
        }
        let Some(node) = &self.node else {
            return match self.ensure_node().await {
                Ok(()) => status(
                    true,
                    "Private ingress is disabled in daemon TOML ([ingress.private]); the apps node is stopped",
                ),
                Err(error) => {
                    tracing::error!(error=?error, "the disabled apps tailnet node could not be removed");
                    status(
                        false,
                        "Removing the apps node is not confirmed; private routes may still be reachable from the tailnet. See daemon logs for details.",
                    )
                }
            };
        };
        if !gateway {
            return status(
                false,
                "The gateway is unavailable, and with it the private listener. See ingress health",
            );
        }
        if let Err(error) = self.check_tailnet_pools().await {
            tracing::error!(error=?error, "the private listener stays off");
            return match error.downcast_ref::<node::TailnetOverlap>() {
                Some(overlap) => status(false, &overlap.to_string()),
                None => status(
                    false,
                    "The private listener stays off: Swarm's address pools could not be verified outside the tailnet ranges. See daemon logs for details.",
                ),
            };
        }
        if let Err(error) = self.ensure_node().await {
            tracing::error!(error=?error, "apps tailnet node is unavailable");
            return status(
                false,
                "Apps node unavailable: could not run its container. See daemon logs for details.",
            );
        }
        self.node_status(node).await.unwrap_or_else(|error| {
            tracing::warn!(error=?error, "apps tailnet node status is unavailable");
            status(false, "Apps node starting: its LocalAPI did not answer yet. See daemon logs if this persists.")
        })
    }

    /// Recomputes per-route status, probing up to four routes concurrently. When
    /// ingress is enabled, only routes the gateway has acknowledged are probed and
    /// the rest stay pending; when disabled, routes report disabled (or failed while
    /// shutdown is unconfirmed).
    async fn probe_routes(&self) {
        let (Ok(table), Ok(everything)) = (
            self.store.served_table().await,
            self.store.routing_table().await,
        ) else {
            return;
        };
        let exact = &self.exact_hostnames(&everything);
        let (healthy, private) = {
            let health = self.health.read().await;
            (health.healthy, health.private.clone())
        };
        let routes: Vec<_> = table
            .into_iter()
            .flat_map(|(id, routes)| routes.into_iter().map(move |route| (id.clone(), route)))
            .collect();
        let private = &private;
        let mut probes = stream::iter(routes)
            .map(|(id, route)| async move {
                let (state, mut message) =
                    self.route_state(&id, &route, exact, healthy, private).await;
                let dns = self.route_dns(&route.hostname);
                // Until the route is ready, a DNS conflict or failure explains it.
                if let Some(problem) = dns.message.filter(|_| state != "ready") {
                    message = format!("DNS records ({}): {problem}", dns.state);
                }
                RouteStatus {
                    environment_id: id.to_string(),
                    name: route.name.clone(),
                    hostname: route.hostname.to_string(),
                    visibility: route.visibility,
                    dns_state: dns.state,
                    dns: match (route.visibility, &self.tunnel) {
                        (Visibility::Public, None) => DnsRecords::ServerAddresses {
                            addresses: self
                                .public_addresses
                                .iter()
                                .map(ToString::to_string)
                                .collect(),
                        },
                        (Visibility::Public, Some(tunnel)) => DnsRecords::TunnelCname {
                            target: tunnel.hostname(),
                        },
                        (Visibility::Private, _) => DnsRecords::TailnetAddresses {
                            addresses: private.addresses.clone(),
                        },
                    },
                    target: route.target,
                    state: state.into(),
                    message,
                }
            })
            .buffer_unordered(4);
        let mut statuses = Vec::new();
        while let Some(status) = probes.next().await {
            statuses.push(status);
        }
        statuses.sort_by(|a, b| a.hostname.cmp(&b.hostname));
        self.health.write().await.routes = statuses;
    }

    /// One route's state and message. Private routes are disabled without
    /// private ingress, fail without a certificate, and wait for the apps node.
    async fn route_state(
        &self,
        id: &EnvironmentId,
        route: &ValidatedRoute,
        exact: &[Hostname],
        healthy: bool,
        private: &PrivateIngressStatus,
    ) -> (&'static str, String) {
        if !self.enabled {
            return if healthy {
                (
                    "disabled",
                    "Ingress is disabled in daemon configuration".into(),
                )
            } else {
                ("failed", "Gateway shutdown is not confirmed; routing may still be active. See ingress health".into())
            };
        }
        let is_private = route.visibility == Visibility::Private;
        if is_private && self.node.is_none() {
            return if private.healthy {
                ("disabled", "Private ingress is disabled in daemon TOML ([ingress.private]), so this private route is not served".into())
            } else {
                ("failed", private.message.clone())
            };
        }
        // Only routes needing a DNS-01 certificate can lack one.
        if let Some(problem) = self.certificates.problem(&route.hostname) {
            return ("failed", format!("No DNS-01 certificate: {problem}"));
        }
        // A broken app must not hide verified readiness for unrelated
        // routes. Only probe destinations acknowledged by the gateway.
        if !self
            .store
            .applied_routes(id)
            .await
            .is_ok_and(|applied| applied.contains(route))
        {
            return (
                "pending",
                "Waiting for the gateway configuration to be applied".into(),
            );
        }
        if is_private && !private.healthy {
            return ("pending", private.message.clone());
        }
        let result = async {
            let hostname = route
                .hostname
                .served(exact)
                .context("every one-character name below the wildcard has its own exact route")?;
            if is_private {
                self.probe_private(&hostname, &private.addresses).await
            } else {
                self.probe_https(&self.client, &format!("https://{hostname}"))
                    .await
            }
        }
        .await;
        match result {
            Ok(()) if is_private => ("ready", "DNS points at the apps node and the private listener serves trusted HTTPS; the tailnet hop and backend health are reported separately".into()),
            Ok(()) if self.tunnel.is_some() => ("ready", "DNS and trusted HTTPS through the Cloudflare Tunnel verified from this daemon; backend health is reported separately".into()),
            Ok(()) => ("ready", "DNS and trusted HTTPS verified from this daemon; backend health is reported separately".into()),
            Err(error) => {
                tracing::debug!(hostname=%route.hostname, visibility=%route.visibility, error=?error, "HTTPS is not ready");
                let message = match &self.tunnel {
                    _ if is_private => "Private HTTPS is not verified yet. Check that DNS A/AAAA records point at the apps node's tailnet addresses, and DNS-01 certificate diagnostics in daemon logs".into(),
                    Some(tunnel) => format!("Public HTTPS is not verified yet. Check the proxied CNAME record to {}, the tunnel's connection state, and daemon logs", tunnel.hostname()),
                    None => "Public HTTPS is not verified yet. Check DNS A/AAAA records, inbound ports 80/443, and Caddy certificate diagnostics in daemon logs".into(),
                };
                ("pending", message)
            }
        }
    }

    /// Verifies a private route without crossing the tailnet:
    ///
    /// 1. Public DNS must answer exactly the apps node's tailnet addresses.
    /// 2. The private listener, reached over the edge network with a PROXY
    ///    header naming a tailnet client and the hostname as SNI, must serve
    ///    a trusted certificate and this installation's probe endpoint.
    async fn probe_private(&self, hostname: &Hostname, addresses: &[String]) -> Result<()> {
        let expected: BTreeSet<IpAddr> = addresses
            .iter()
            .filter_map(|address| address.parse().ok())
            .collect();
        let resolved: BTreeSet<IpAddr> = tokio::net::lookup_host((hostname.as_str(), 443))
            .await
            .with_context(|| format!("resolve {hostname}"))?
            .map(|address| address.ip())
            .collect();
        anyhow::ensure!(
            !expected.is_empty() && resolved == expected,
            "DNS answers {resolved:?} instead of the apps node's addresses {expected:?}"
        );
        let (relay, _relay) = node::proxy_relay(self.private_listener().await?, PROBE_CLIENT)
            .await
            .context("start the private probe's PROXY relay")?;
        let client = Self::probe_client()
            .resolve(hostname.as_str(), relay)
            .build()?;
        self.probe_https(&client, &format!("https://{hostname}:{}", relay.port()))
            .await
    }

    /// Verifies DNS and trusted TLS by fetching the gateway's probe endpoint at
    /// `origin` and requiring this instance's identity (at most 256 bytes) as
    /// the body.
    ///
    /// ```text
    /// GET https://<hostname>/.well-known/piqueld-ingress  ->  200 "<instance_id>"
    /// ```
    async fn probe_https(&self, client: &reqwest::Client, origin: &str) -> Result<()> {
        let response = client
            .get(format!("{origin}/.well-known/piqueld-ingress"))
            .send()
            .await?;
        anyhow::ensure!(
            response.status().is_success(),
            "HTTPS probe returned {}",
            response.status()
        );
        let mut body = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = body.next().await {
            let chunk = chunk?;
            anyhow::ensure!(
                bytes.len() + chunk.len() <= 256,
                "HTTPS probe body exceeds its limit"
            );
            bytes.extend(chunk);
        }
        anyhow::ensure!(
            bytes == self.instance_id.as_bytes(),
            "DNS points at another server or gateway"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests;
