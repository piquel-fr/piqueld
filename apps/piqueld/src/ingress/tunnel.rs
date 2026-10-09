//! The Cloudflare Tunnel: a `cloudflared` container on the gateway's edge
//! network that carries public routes' traffic in tunnel mode, so the host
//! publishes no ports. Cloudflare terminates TLS; `cloudflared` connects out
//! to it and forwards every request to the gateway's tunnel listener.
//!
//! The tunnel is locally managed: piqueld writes `cloudflared`'s configuration
//! with one catch-all rule, so adding or removing routes never touches
//! Cloudflare. Hostnames the gateway does not know receive Caddy's 404.
//!
//! ```text
//! Cloudflare -> cloudflared -> http://<gateway>:8080 (public routes only)
//! ```
//!
//! `cloudflared` runs as the daemon's user with no capabilities and a
//! read-only root, and keeps running across daemon restarts. Its files live
//! under `<data_dir>/ingress/tunnel`, mounted read-only:
//!
//! ```text
//! config.json         configuration, owned by piqueld
//! credentials.json    the tunnel's credentials (0600)
//! ```
//!
//! Like the gateway's, changes are daemon-scoped journal actions, and a
//! container whose spec hash differs is replaced. Its connection state comes
//! from a Docker healthcheck running `cloudflared tunnel ready` inside the
//! container, which checks its metrics server's `/ready`. The daemon reads the
//! outcome through the Docker API, so it never needs a route to the container,
//! and the metrics server listens only on the container's loopback.

use super::{CLOUDFLARED_IMAGE, Ingress};
use crate::config::TunnelCredentials;
use anyhow::{Context, Result};
use piqueld_core::api::PublicIngressStatus;
use serde_json::{Value, json};
use std::path::PathBuf;

/// Port of the gateway's tunnel listener, never published on the host.
pub(super) const TUNNEL_PORT: u16 = 8080;
/// `cloudflared`'s metrics server, which serves `/ready` on the container's
/// loopback only, for its healthcheck.
const METRICS: &str = "127.0.0.1:2000";
/// Marks the container with a fingerprint of its credentials, so new
/// credentials replace the container.
const CREDENTIALS_LABEL: &str = "io.piqueld.ingress-tunnel-credentials";
/// Where the container reads its files.
const MOUNT: &str = "/etc/cloudflared";

impl Ingress {
    /// The `cloudflared` container's name.
    pub(super) fn tunnel_name(&self) -> String {
        format!("{}-cloudflared", self.name)
    }

    /// `<data_dir>/ingress/tunnel`.
    fn tunnel_directory(&self) -> PathBuf {
        self.directory.join("tunnel")
    }

    /// `cloudflared`'s configuration, with a single catch-all rule to the
    /// gateway's tunnel listener. JSON is valid YAML, which `cloudflared` reads.
    ///
    /// ```text
    /// {"tunnel":"<id>","credentials-file":"/etc/cloudflared/credentials.json",
    ///  "metrics":"127.0.0.1:2000","ingress":[{"service":"http://<gateway>:8080"}]}
    /// ```
    pub(super) fn tunnel_configuration(&self, credentials: &TunnelCredentials) -> Value {
        json!({
            "tunnel":credentials.id.to_string(),
            "credentials-file":format!("{MOUNT}/credentials.json"),
            "metrics":METRICS,
            "ingress":[{"service":format!("http://{}:{TUNNEL_PORT}", self.name)}]
        })
    }

    /// Builds the hardened `cloudflared` container spec: the daemon's user, a
    /// read-only root, no capabilities, only the edge network, no published
    /// ports, and its directory bind-mounted read-only. Its healthcheck fails
    /// until a connection to Cloudflare is up; failures in the first 30 s are
    /// ignored while it connects, checked every 2 s. Like the gateway's, the
    /// spec's own hash is stored in `CONFIGURATION_LABEL` to detect drift.
    pub(super) fn tunnel_spec(&self, credentials: &TunnelCredentials) -> Value {
        let uid = rustix::process::geteuid().as_raw();
        let gid = rustix::process::getegid().as_raw();
        let mut labels = self.labels();
        labels[CREDENTIALS_LABEL] = Self::fingerprint(&credentials.file).into();
        let mut spec = json!({
            "Image":CLOUDFLARED_IMAGE,"User":format!("{uid}:{gid}"),
            "Cmd":["tunnel","--config",format!("{MOUNT}/config.json"),"run"],
            "Env":[],
            "Labels":labels,
            "Healthcheck":{
                "Test":["CMD","cloudflared","tunnel","--metrics",METRICS,"ready"],
                "Interval":10_000_000_000_u64,"Timeout":5_000_000_000_u64,"Retries":1,
                "StartPeriod":30_000_000_000_u64,"StartInterval":2_000_000_000_u64
            },
            "HostConfig":{
                "Binds":[format!("{}:{MOUNT}:ro", self.tunnel_directory().display())],
                "RestartPolicy":{"Name":"unless-stopped"},
                "ReadonlyRootfs":true,"CapDrop":["ALL"],"SecurityOpt":["no-new-privileges:true"],
                "LogConfig":{"Type":"local","Config":{"max-size":"10m","max-file":"3"}},
                "NetworkMode":self.name
            }
        });
        Self::label_hash(&mut spec);
        spec
    }

    /// Ensures a running `cloudflared` matching the current spec. While the
    /// tunnel is disabled, removes a leftover container and its credentials.
    /// Runs after the gateway, whose edge network it joins.
    pub(super) async fn ensure_tunnel(&self) -> Result<()> {
        let name = self.tunnel_name();
        let Some(credentials) = &self.tunnel else {
            // Each is attempted even if the other fails.
            let removed = self.remove_stale_container(&name).await;
            return removed.and(self.remove_tunnel_credentials().await);
        };
        self.ensure_container(
            &name,
            &self.tunnel_spec(credentials),
            "ingress_start_tunnel",
            self.prepare_tunnel(credentials),
        )
        .await
    }

    /// Removes the credentials written for `cloudflared`, whenever it is not
    /// meant to run.
    pub(super) async fn remove_tunnel_credentials(&self) -> Result<()> {
        match tokio::fs::remove_file(self.tunnel_directory().join("credentials.json")).await {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(error).context("remove the tunnel credentials written for cloudflared")
            }
            _ => Ok(()),
        }
    }

    /// Writes the tunnel's configuration and credentials, and pulls its image.
    async fn prepare_tunnel(&self, credentials: &TunnelCredentials) -> Result<()> {
        let directory = self.tunnel_directory();
        crate::prepare_data_dir(&directory).await?;
        super::certificates::write_private(
            &directory.join("config.json"),
            &serde_json::to_vec(&self.tunnel_configuration(credentials))?,
        )
        .await?;
        super::certificates::write_private(
            &directory.join("credentials.json"),
            credentials.file.expose().as_bytes(),
        )
        .await?;
        self.ensure_image(CLOUDFLARED_IMAGE).await
    }

    /// How public routes arrive: directly, or through the tunnel with its
    /// connection state.
    pub(super) async fn public_status(&self) -> PublicIngressStatus {
        let Some(credentials) = &self.tunnel else {
            return PublicIngressStatus::Direct;
        };
        let (connected, message) = if self.enabled {
            match self.tunnel_health().await.as_deref() {
                Ok("healthy") => (
                    true,
                    "cloudflared is connected to Cloudflare and forwards to the tunnel listener",
                ),
                Ok("starting") => (
                    false,
                    "cloudflared is starting and not connected to Cloudflare yet",
                ),
                Ok(_) => (
                    false,
                    "cloudflared is not connected to Cloudflare. Check the tunnel credentials and outbound connectivity in daemon logs",
                ),
                Err(error) => {
                    tracing::warn!(error=?error, "cloudflared's state is unavailable");
                    (
                        false,
                        "cloudflared is not running, or its state could not be read. See daemon logs for details.",
                    )
                }
            }
        } else {
            (
                false,
                "Ingress is disabled in daemon TOML; cloudflared is stopped",
            )
        };
        PublicIngressStatus::Tunnel {
            id: credentials.id.to_string(),
            connected,
            message: message.into(),
        }
    }

    /// Docker's health status of the running `cloudflared`, from its
    /// healthcheck: `starting`, `healthy` or `unhealthy`.
    async fn tunnel_health(&self) -> Result<String> {
        let container = self
            .named_container(&self.tunnel_name())
            .await?
            .context("cloudflared does not exist")?;
        anyhow::ensure!(
            container["State"]["Running"] == true,
            "cloudflared is not running"
        );
        container["State"]["Health"]["Status"]
            .as_str()
            .map(str::to_owned)
            .context("cloudflared reports no health status")
    }
}
