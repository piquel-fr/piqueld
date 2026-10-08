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
//! container whose spec hash differs is replaced. Its readiness comes from
//! its metrics server's `/ready`, queried over the edge network.

use super::{CLOUDFLARED_IMAGE, Ingress};
use crate::config::TunnelCredentials;
use anyhow::{Context, Result};
use piqueld_core::api::PublicIngressStatus;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{net::SocketAddr, path::PathBuf};

/// Port of the gateway's tunnel listener, never published on the host.
pub(super) const TUNNEL_PORT: u16 = 8080;
/// `cloudflared`'s metrics server, which serves `/ready` on the edge network.
const METRICS_PORT: u16 = 2000;
/// Marks the container with a fingerprint of its credentials, so new
/// credentials replace the container.
const CREDENTIALS_LABEL: &str = "io.piqueld.ingress-tunnel-credentials";
/// Where the container reads its files.
const MOUNT: &str = "/etc/cloudflared";

/// The field piqueld reads from `/ready`, which answers 200 once at least one
/// connection to Cloudflare's edge is up and 503 with the same body before.
///
/// ```text
/// {"status":503,"readyConnections":0,"connectorId":"4cb8f2c2-..."}
/// ```
#[derive(Deserialize)]
struct Ready {
    #[serde(rename = "readyConnections")]
    connections: u32,
}

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
    ///  "metrics":"0.0.0.0:2000","ingress":[{"service":"http://<gateway>:8080"}]}
    /// ```
    pub(super) fn tunnel_configuration(&self, credentials: &TunnelCredentials) -> Value {
        json!({
            "tunnel":credentials.id.to_string(),
            "credentials-file":format!("{MOUNT}/credentials.json"),
            "metrics":format!("0.0.0.0:{METRICS_PORT}"),
            "ingress":[{"service":format!("http://{}:{TUNNEL_PORT}", self.name)}]
        })
    }

    /// Builds the hardened `cloudflared` container spec: the daemon's user, a
    /// read-only root, no capabilities, only the edge network, no published
    /// ports, and its directory bind-mounted read-only. Like the gateway's,
    /// the spec's own hash is stored in `CONFIGURATION_LABEL` to detect drift.
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
    /// connections to Cloudflare's edge.
    pub(super) async fn public_status(&self) -> PublicIngressStatus {
        let Some(credentials) = &self.tunnel else {
            return PublicIngressStatus::Direct;
        };
        let (connections, message) = if self.enabled {
            match self.tunnel_connections().await {
                Ok(0) => (
                    0,
                    "cloudflared is not connected to Cloudflare. Check the tunnel credentials and outbound connectivity in daemon logs",
                ),
                Ok(connections) => (
                    connections,
                    "cloudflared is connected to Cloudflare and forwards to the tunnel listener",
                ),
                Err(error) => {
                    tracing::warn!(error=?error, "cloudflared readiness is unavailable");
                    (
                        0,
                        "cloudflared's readiness could not be read. See daemon logs for details.",
                    )
                }
            }
        } else {
            (
                0,
                "Ingress is disabled in daemon TOML; cloudflared is stopped",
            )
        };
        PublicIngressStatus::Tunnel {
            id: credentials.id.to_string(),
            connections,
            message: message.into(),
        }
    }

    /// `cloudflared`'s ready connections, from `/ready` on its edge address.
    async fn tunnel_connections(&self) -> Result<u32> {
        let address = SocketAddr::new(self.edge_address(&self.tunnel_name()).await?, METRICS_PORT);
        let ready: Ready = self
            .client
            .get(format!("http://{address}/ready"))
            .send()
            .await
            .context("query cloudflared's /ready")?
            .json()
            .await
            .context("decode cloudflared's /ready")?;
        Ok(ready.connections)
    }
}
