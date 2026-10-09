//! Installation-owned Caddy gateway, independent of the daemon's process
//! lifetime. Public routes are served on its published listener, or on a
//! tunnel listener that only `cloudflared` reaches; private routes on a
//! private listener that only the apps tailnet node reaches.
mod certificates;
mod configuration;
mod gateway;
mod node;
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
    manifest::{Hostname, ValidatedRoute, Visibility},
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
    /// Docker client used only for streaming image pulls.
    images: bollard::Docker,
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
    /// The Cloudflare Tunnel serving public routes instead of ports 80/443,
    /// while `[ingress.tunnel]` is enabled.
    tunnel: Option<TunnelCredentials>,
    /// Set once Swarm's address pools are verified outside the tailnet ranges.
    tailnet_pools: tokio::sync::OnceCell<()>,
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
        let suffix = format!("{:x}", Sha256::digest(store.instance_id().as_bytes()));
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
            instance_id: store.instance_id().into(),
            name: format!("piqueld-ingress-{}", &suffix[..16]),
            caddy: UnixApi::new(directory.join("control/admin.sock"), request_timeout),
            directory,
            docker: UnixApi::new(socket.into(), request_timeout),
            images: bollard::Docker::connect_with_unix(
                socket.to_str().context("Docker socket is not UTF-8")?,
                request_timeout.as_secs(),
                bollard::API_DEFAULT_VERSION,
            )?,
            client: Self::probe_client().build()?,
            store,
            update: Mutex::new(()),
            requester: Mutex::new(None),
            health: RwLock::new(IngressStatus {
                enabled,
                healthy: false,
                message: "Waiting for gateway reconciliation".into(),
                public: PublicIngressStatus::default(),
                private: PrivateIngressStatus::default(),
                routes: Vec::new(),
            }),
            logs_since: Mutex::default(),
            certificates,
            node: None,
            tunnel: None,
            tailnet_pools: tokio::sync::OnceCell::new(),
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
        .with_tunnel(&config.ingress.tunnel))
    }

    /// Serves private routes through the apps tailnet node when
    /// `[ingress.private]` enables it.
    #[must_use]
    pub fn with_private(mut self, config: &PrivateIngressConfig) -> Self {
        self.node = node::Node::new(config, &self.directory);
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

    /// Configures DNS providers and the ACME account for DNS-01 certificates.
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

    /// Repairs lifecycle/configuration and probes public HTTPS independently of deployments.
    pub async fn run(&self, cancellation: CancellationToken) {
        tokio::join!(
            self.run_gateway(&cancellation),
            self.run_probes(&cancellation),
            self.run_certificates(&cancellation)
        );
    }

    /// Hostnames served with DNS-01 certificates instead of Caddy's automatic
    /// HTTPS: private routes, which a public CA cannot reach, while private
    /// ingress is enabled.
    fn dns01_hostnames(&self, table: &RoutingTable) -> BTreeSet<Hostname> {
        if self.node.is_none() {
            return BTreeSet::new();
        }
        table
            .values()
            .flatten()
            .filter(|route| route.visibility == Visibility::Private)
            .map(|route| route.hostname.clone())
            .collect()
    }

    /// Every minute, issues and renews the DNS-01 certificates deployed routes
    /// need. The gateway loop loads new certificates into Caddy.
    async fn run_certificates(&self, cancellation: &CancellationToken) {
        let mut tick = tokio::time::interval(Duration::from_mins(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { ()=cancellation.cancelled()=>return, _=tick.tick()=>{} }
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

    /// Every 10s, reconciles the gateway under the writer lock and relays Caddy
    /// logs. Failures are logged and retried on the next tick.
    async fn run_gateway(&self, cancellation: &CancellationToken) {
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { ()=cancellation.cancelled()=>return, _=tick.tick()=>{} }
            tokio::select! {
                ()=cancellation.cancelled()=>return,
                ()=async {
                    let result = {
                        let _guard = self.update.lock().await;
                        *self.requester.lock().await = None;
                        self.synchronize().await
                    };
                    if let Err(error) = result { tracing::warn!(error=?error,"ingress reconciliation failed; will retry"); }
                    if let Err(error) = self.relay_logs().await { tracing::debug!(?error,"could not read Caddy diagnostics"); }
                }=>{}
            }
        }
    }

    /// Every 15s, refreshes route status by probing HTTPS for acknowledged routes.
    async fn run_probes(&self, cancellation: &CancellationToken) {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { ()=cancellation.cancelled()=>return, _=tick.tick()=>{} }
            // Slow public DNS/TLS must never delay gateway drift repair or disablement.
            tokio::select! { ()=cancellation.cancelled()=>return, ()=self.probe_routes()=>{} }
        }
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
    /// call. Node failures degrade only the private listener's health.
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
            let table = self.store.routing_table().await?;
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
        // Health transitions become history, and sustained failures notify
        // administrators, like other daemon dependencies.
        if let Err(error) = self
            .store
            .observe_condition("ingress_unavailable", None, !healthy, now_ms(), 45_000)
            .await
        {
            tracing::error!(error=?error, "ingress health observation could not be recorded");
        }
        let mut health = self.health.write().await;
        health.healthy = healthy;
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
        let Ok(table) = self.store.routing_table().await else {
            return;
        };
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
                let (state, message) = self.route_state(&id, &route, healthy, private).await;
                RouteStatus {
                    environment_id: id.to_string(),
                    hostname: route.hostname.to_string(),
                    visibility: route.visibility,
                    dns: match (route.visibility, &self.tunnel) {
                        (Visibility::Public, None) => DnsRecords::ServerAddresses,
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
        if is_private {
            if self.node.is_none() {
                return if private.healthy {
                    ("disabled", "Private ingress is disabled in daemon TOML ([ingress.private]), so this private route is not served".into())
                } else {
                    ("failed", private.message.clone())
                };
            }
            if let Some(problem) = self.certificates.problem(&route.hostname) {
                return ("failed", format!("No DNS-01 certificate: {problem}"));
            }
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
        let result = if is_private {
            self.probe_private(&route.hostname, &private.addresses)
                .await
        } else {
            self.probe_https(&self.client, &format!("https://{}", route.hostname))
                .await
        };
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
