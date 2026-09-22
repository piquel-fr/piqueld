//! Installation-owned Caddy gateway, independent of the daemon's process lifetime.
mod configuration;
mod gateway;
mod wire;

use crate::store::{Store, ingress::RoutingTable};
use anyhow::{Context, Result};
use futures_util::{StreamExt, stream};
use piqueld_core::{
    api::{IngressStatus, RouteStatus},
    manifest::ValidatedRoute,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use wire::UnixApi;

/// Version released with piqueld; upgrades deliberately replace the gateway.
pub const CADDY_IMAGE: &str = "caddy:2.11.4-alpine";

/// Serializes gateway configuration, lifecycle, and durable route projection.
pub struct Ingress {
    pub(crate) enabled: bool,
    instance_id: String,
    name: String,
    directory: PathBuf,
    docker: UnixApi,
    caddy: UnixApi,
    images: bollard::Docker,
    client: reqwest::Client,
    store: Arc<Store>,
    update: Mutex<()>,
    health: RwLock<IngressStatus>,
    logs_since: Mutex<u64>,
    #[cfg(test)]
    issuer: Option<serde_json::Value>,
}

impl Ingress {
    /// Creates an inert manager. Startup failures are reported by its background loop.
    /// # Errors
    /// Returns invalid local client configuration errors.
    pub fn new(enabled: bool, socket: &Path, data_dir: &Path, store: Arc<Store>) -> Result<Self> {
        use sha2::{Digest, Sha256};
        let suffix = format!("{:x}", Sha256::digest(store.instance_id().as_bytes()));
        let directory = data_dir.join("ingress");
        // Local Docker and Caddy control requests share the daemon's request budget.
        // Deployment convergence still imposes its configured outer deadline.
        let request_timeout = crate::docker::DockerTimeout::Request.duration();
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
            health: RwLock::new(IngressStatus {
                enabled,
                healthy: false,
                message: "Waiting for gateway reconciliation".into(),
                routes: Vec::new(),
            }),
            logs_since: Mutex::new(0),
            #[cfg(test)]
            issuer: None,
        })
    }

    /// Latest bounded background health snapshot; this never waits for Docker.
    pub async fn status(&self) -> IngressStatus {
        self.health.read().await.clone()
    }

    /// Applies one application's deployment boundary under the gateway writer lock.
    pub(crate) async fn apply(
        &self,
        operation: &piqueld_core::Operation,
        routes: &[ValidatedRoute],
        ready: bool,
    ) -> Result<()> {
        let _guard = self.update.lock().await;
        let id = &operation.application_id;
        let previous = self
            .store
            .routing_table()
            .await?
            .remove(id)
            .unwrap_or_default();
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
        self.synchronize().await
    }

    /// Repairs lifecycle/configuration and probes public HTTPS independently of deployments.
    pub async fn run(&self, cancellation: CancellationToken) {
        tokio::join!(
            self.run_gateway(&cancellation),
            self.run_probes(&cancellation)
        );
    }

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
                        self.synchronize().await
                    };
                    if let Err(error) = result { tracing::warn!(error=?error,"ingress reconciliation failed; will retry"); }
                    if let Err(error) = self.relay_logs().await { tracing::debug!(?error,"could not read Caddy diagnostics"); }
                }=>{}
            }
        }
    }

    async fn run_probes(&self, cancellation: &CancellationToken) {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { ()=cancellation.cancelled()=>return, _=tick.tick()=>{} }
            // Slow public DNS/TLS must never delay gateway drift repair or disablement.
            tokio::select! { ()=cancellation.cancelled()=>return, ()=self.probe_routes()=>{} }
        }
    }

    async fn synchronize(&self) -> Result<()> {
        let result = async {
            let table = self
                .store
                .routing_table()
                .await
                .context("read deployed routing configuration")?;
            self.synchronize_table(&table).await
        }
        .await;
        let mut health = self.health.write().await;
        health.healthy = result.is_ok();
        health.message = match &result {
            Ok(()) if self.enabled => {
                "Caddy is running and routing configuration is applied".into()
            }
            Ok(()) => "Ingress is disabled in daemon TOML; public listeners are stopped".into(),
            Err(error) => {
                tracing::error!(error=?error, "managed ingress is unhealthy");
                // Context strings describe the operation; detailed Docker/Caddy
                // response bodies stay in logs rather than becoming UI content.
                format!(
                    "Ingress unavailable: {}. See daemon logs for details.",
                    error.to_string().chars().take(160).collect::<String>()
                )
            }
        };
        result
    }

    async fn synchronize_table(&self, table: &RoutingTable) -> Result<()> {
        if self.enabled {
            self.ensure_gateway(table)
                .await
                .context("prepare the Caddy gateway (requires free ports 80/443 and Docker 28+)")?;
            self.configure_gateway(table)
                .await
                .context("apply Caddy routes and network attachments")?;
        } else {
            self.stop_gateway()
                .await
                .context("stop the disabled Caddy gateway")?;
        }
        self.store
            .acknowledge_routes(table)
            .await
            .context("record accepted gateway configuration")
    }

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
                application_id: id.to_string(), hostname: route.hostname.to_string(), service: route.service.to_string(), port: route.port.get(),
                state: "disabled".into(), message: "Ingress is disabled in daemon configuration".into(),
            };
            if !self.enabled && !healthy {
                status.state = "failed".into();
                status.message = "Gateway shutdown is not confirmed; public routing may still be active. See ingress health".into();
            }
            if self.enabled {
                status.state = "pending".into();
                status.message = "Waiting for the gateway configuration to be applied".into();
                if healthy {
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
