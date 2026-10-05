//! Installation-owned Caddy gateway, independent of the daemon's process lifetime.
mod certificates;
mod configuration;
mod gateway;
mod wire;

use crate::{
    config::{AcmeConfig, DnsConfig},
    dns::Dns,
    store::{Store, ingress::RoutingTable, now_ms},
};
use anyhow::{Context, Result};
use certificates::Certificates;
use futures_util::{StreamExt, stream};
use piqueld_core::{
    api::{IngressStatus, RouteStatus},
    manifest::{Hostname, ValidatedRoute},
};
use std::{
    collections::BTreeSet,
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
    /// Unix timestamp (seconds) up to which Caddy logs were already relayed.
    logs_since: Mutex<u64>,
    /// DNS providers and the DNS-01 certificates loaded into Caddy.
    certificates: Certificates,
    #[cfg(test)]
    issuer: Option<serde_json::Value>,
    #[cfg(test)]
    extra_hosts: Vec<String>,
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
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .build()?,
            store,
            update: Mutex::new(()),
            requester: Mutex::new(None),
            health: RwLock::new(IngressStatus {
                enabled,
                healthy: false,
                message: "Waiting for gateway reconciliation".into(),
                routes: Vec::new(),
            }),
            logs_since: Mutex::new(0),
            certificates,
            #[cfg(test)]
            issuer: None,
            #[cfg(test)]
            extra_hosts: Vec::new(),
        })
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
    /// ready yet and no hostname is being withdrawn. Fails for this application's
    /// own network failures, not for other applications'.
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
                .all(|old| routes.iter().any(|new| new.hostname == old.hostname))
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
    /// HTTPS. Only private routes (#164) need them, and no route is private yet.
    fn dns01_hostnames(_table: &RoutingTable) -> BTreeSet<Hostname> {
        BTreeSet::new()
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
                    Ok(table) => Self::dns01_hostnames(&table),
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

    /// Every 15s, refreshes route status by probing public HTTPS for acknowledged routes.
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
    /// applies routes and attachments, then acknowledges the applied table. When
    /// disabled: stops the gateway and acknowledges the withdrawal. Callers must
    /// hold the writer lock. Network failures degrade health; they fail the call
    /// only for `application` (or for any application when `None`).
    async fn synchronize_for(
        &self,
        application: Option<&piqueld_core::EnvironmentId>,
    ) -> Result<()> {
        let mut failures = std::collections::BTreeMap::new();
        // Health messages name only the failed stage. Error details, which may
        // include Docker/Caddy response bodies, stay in daemon logs.
        let mut stage = "read deployed routing configuration";
        let result: Result<()> = async {
            let table = self.store.routing_table().await?;
            if self.enabled {
                stage = "verify application ingress networks";
                let (accepted, networks, rejected) = self.prepare_routes(&table).await?;
                failures = rejected;
                stage = "prepare the Caddy gateway (requires free ports 80/443 and Docker 28+)";
                self.ensure_gateway(&accepted, &networks).await?;
                stage = "apply Caddy routes and network attachments";
                self.configure_gateway(&accepted, &networks).await?;
                stage = "record applied routes";
                self.store.acknowledge_routes(&accepted).await?;
            } else {
                stage = "stop the disabled Caddy gateway";
                self.stop_gateway().await?;
                stage = "record withdrawn routes";
                self.store.acknowledge_routes(&table).await?;
            }
            anyhow::Ok(())
        }
        .await
        .context(stage);
        let healthy = result.is_ok() && failures.is_empty();
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
        health.message = match &result {
            Ok(()) if !failures.is_empty() => format!(
                "Ingress degraded: {} application network(s) unavailable. See daemon logs for details.",
                failures.len()
            ),
            Ok(()) if self.enabled => {
                "Caddy is running and routing configuration is applied".into()
            }
            Ok(()) => "Ingress is disabled in daemon TOML; public listeners are stopped".into(),
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

    /// Recomputes per-route status, probing up to four routes concurrently. When
    /// ingress is enabled, only routes the gateway has acknowledged are probed and
    /// the rest stay pending; when disabled, routes report disabled (or failed while
    /// shutdown is unconfirmed).
    async fn probe_routes(&self) {
        let Ok(table) = self.store.routing_table().await else {
            return;
        };
        let healthy = self.health.read().await.healthy;
        let routes: Vec<_> = table
            .into_iter()
            .flat_map(|(id, routes)| routes.into_iter().map(move |route| (id.clone(), route)))
            .collect();
        let mut probes = stream::iter(routes).map(|(id,route)| async move {
            let mut status = RouteStatus {
                environment_id: id.to_string(), hostname: route.hostname.to_string(), target: route.target.clone(),
                state: "disabled".into(), message: "Ingress is disabled in daemon configuration".into(),
            };
            if !self.enabled && !healthy {
                status.state = "failed".into();
                status.message = "Gateway shutdown is not confirmed; public routing may still be active. See ingress health".into();
            }
            if self.enabled {
                status.state = "pending".into();
                status.message = "Waiting for the gateway configuration to be applied".into();
                // A broken app must not hide verified readiness for unrelated
                // routes. Only probe destinations acknowledged by the gateway.
                if self.store.applied_routes(&id).await.is_ok_and(|applied| applied.contains(&route)) {
                    match self.probe_https(route.hostname.as_str()).await {
                        Ok(()) => { status.state="ready".into(); status.message="DNS and trusted HTTPS verified from this daemon; backend health is reported separately".into(); }
                        Err(error) => {
                            tracing::debug!(hostname=%route.hostname,error=?error,"public HTTPS is not ready");
                            status.message="Public HTTPS is not verified yet. Check DNS A/AAAA records, inbound ports 80/443, and Caddy certificate diagnostics in daemon logs".into();
                        }
                    }
                }
            }
            status
        }).buffer_unordered(4);
        let mut statuses = Vec::new();
        while let Some(status) = probes.next().await {
            statuses.push(status);
        }
        statuses.sort_by(|a, b| a.hostname.cmp(&b.hostname));
        self.health.write().await.routes = statuses;
    }

    /// Verifies DNS and trusted TLS by fetching the gateway's probe endpoint and
    /// requiring this instance's identity (at most 256 bytes) as the body.
    ///
    /// ```text
    /// GET https://<hostname>/.well-known/piqueld-ingress  ->  200 "<instance_id>"
    /// ```
    async fn probe_https(&self, hostname: &str) -> Result<()> {
        let response = self
            .client
            .get(format!("https://{hostname}/.well-known/piqueld-ingress"))
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
