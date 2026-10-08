//! The apps tailnet node: a Tailscale container on the gateway's edge network
//! that carries private routes' traffic. It is separate from the daemon's own
//! node (`crate::tailnet`), which terminates TLS itself and must not depend on
//! Docker: this one passes raw TLS through to Caddy.
//!
//! Its serve configuration forwards tailnet TCP 443 and 80 to the gateway's
//! private listener, prefixed with a PROXY v2 header so Caddy sees each
//! client's tailnet address. `tailscaled` uses userspace networking, so it
//! dials the gateway by its container name like any other process.
//!
//! ```text
//! tailnet :443 -> <gateway>:8443 (HTTPS)    tailnet :80 -> <gateway>:8081 (HTTP -> HTTPS)
//! ```
//!
//! The node runs as the daemon's user with no capabilities and a read-only
//! root, and keeps running across daemon restarts. Its files live under
//! `<data_dir>/ingress/tailscale`:
//!
//! ```text
//! state/                  node identity and preferences
//! run/tailscaled.sock     LocalAPI, read for status
//! config/serve.json       serve configuration, owned by piqueld
//! config/auth-key         first-login auth key (0600), when configured
//! ```
//!
//! Like the gateway's, changes are daemon-scoped journal actions, and a
//! container whose spec hash differs is replaced.

use super::{Ingress, TAILSCALE_IMAGE, gateway::CONFIGURATION_LABEL, wire::UnixApi};
use crate::config::{Credential, PrivateIngressConfig};
use anyhow::{Context, Result};
use hyper::Method;
use piqueld_core::api::PrivateIngressStatus;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{net::IpAddr, path::PathBuf};

/// Ports of the gateway's private listener, never published on the host.
pub(super) const PRIVATE_HTTPS_PORT: u16 = 8443;
pub(super) const PRIVATE_HTTP_PORT: u16 = 8081;

/// The configured apps node.
pub(super) struct Node {
    /// Node name, which becomes `<hostname>.<tailnet>.ts.net`.
    hostname: String,
    /// Auth key for the first login.
    auth_key: Option<Credential>,
    /// The node's `LocalAPI`, over the socket in its `run` directory.
    api: UnixApi,
    /// Login URL last relayed to daemon logs.
    login_url: std::sync::Mutex<Option<String>>,
}

/// The fields piqueld reads from the `LocalAPI` at `/localapi/v0/status`.
#[derive(Deserialize)]
struct LocalStatus {
    #[serde(rename = "BackendState")]
    backend_state: String,
    /// Interactive login URL while the node needs login.
    #[serde(rename = "AuthURL", default)]
    auth_url: String,
    #[serde(rename = "Self")]
    node: Option<LocalNode>,
}

#[derive(Deserialize)]
struct LocalNode {
    #[serde(rename = "DNSName")]
    dns_name: String,
    /// `null` until the node has logged in.
    #[serde(rename = "TailscaleIPs")]
    addresses: Option<Vec<IpAddr>>,
}

impl Node {
    /// The node for `[ingress.private]`, or none while it is disabled.
    /// `directory` is the ingress directory.
    pub(super) fn new(config: &PrivateIngressConfig, directory: &std::path::Path) -> Option<Self> {
        config.enabled.then(|| Self {
            hostname: config.hostname.clone(),
            auth_key: config.auth_key.clone(),
            api: UnixApi::new(
                directory.join("tailscale/run/tailscaled.sock"),
                crate::docker::DockerTimeout::Request.duration(),
            )
            .with_host("local-tailscaled.sock"),
            login_url: std::sync::Mutex::new(None),
        })
    }
}

impl Ingress {
    /// The node's container name.
    pub(super) fn node_name(&self) -> String {
        format!("{}-tailscale", self.name)
    }

    /// `<data_dir>/ingress/tailscale`.
    fn node_directory(&self) -> PathBuf {
        self.directory.join("tailscale")
    }

    /// Forwards tailnet TCP 443 and 80 to the private listener with PROXY v2.
    ///
    /// ```text
    /// {"TCP":{"443":{"TCPForward":"<gateway>:8443","ProxyProtocol":2},"80":{...:8081}}}
    /// ```
    pub(super) fn serve_configuration(&self) -> Value {
        let forward =
            |port: u16| json!({"TCPForward":format!("{}:{port}", self.name),"ProxyProtocol":2});
        json!({"TCP":{"443":forward(PRIVATE_HTTPS_PORT),"80":forward(PRIVATE_HTTP_PORT)}})
    }

    /// Builds the hardened node container spec: the daemon's user, userspace
    /// networking, a read-only root, no capabilities, only the edge network,
    /// and the node's directories bind-mounted. Like the gateway's, the spec's
    /// own hash is stored in `CONFIGURATION_LABEL` to detect drift.
    pub(super) fn node_spec(&self, node: &Node) -> Value {
        let uid = rustix::process::geteuid().as_raw();
        let gid = rustix::process::getegid().as_raw();
        let directory = self.node_directory();
        let binds: Vec<_> = [("state", ""), ("run", ""), ("config", ":ro")]
            .into_iter()
            .map(|(name, mode)| format!("{}:/{name}{mode}", directory.join(name).display()))
            .collect();
        let mut environment = vec![
            "TS_STATE_DIR=/state".to_owned(),
            "TS_SOCKET=/run/tailscaled.sock".to_owned(),
            "TS_USERSPACE=true".to_owned(),
            format!("TS_HOSTNAME={}", node.hostname),
            "TS_SERVE_CONFIG=/config/serve.json".to_owned(),
            "TS_ACCEPT_DNS=false".to_owned(),
            // Later starts reuse the node state instead of the key.
            "TS_AUTH_ONCE=true".to_owned(),
            // An interactive login otherwise times out after a minute, and
            // every restart brings a new login URL.
            "TS_BOOT_TIMEOUT=24h".to_owned(),
        ];
        if node.auth_key.is_some() {
            environment.push("TS_AUTHKEY=file:/config/auth-key".to_owned());
        }
        let mut spec = json!({
            "Image":TAILSCALE_IMAGE,"User":format!("{uid}:{gid}"),
            "Cmd":["/usr/local/bin/containerboot"],
            "Env":environment,
            "Labels":self.labels(),
            "HostConfig":{
                "Binds":binds,"RestartPolicy":{"Name":"unless-stopped"},
                "ReadonlyRootfs":true,"CapDrop":["ALL"],"SecurityOpt":["no-new-privileges:true"],
                "Tmpfs":{"/tmp":"rw,noexec,nosuid,size=16777216"},
                "LogConfig":{"Type":"local","Config":{"max-size":"10m","max-file":"3"}},
                "NetworkMode":self.name
            }
        });
        Self::label_hash(&mut spec);
        spec
    }

    /// Ensures a running node matching the current spec, or none while
    /// private ingress is disabled. Runs after the gateway, whose edge network
    /// the node joins.
    pub(super) async fn ensure_node(&self) -> Result<()> {
        let name = self.node_name();
        let Some(node) = &self.node else {
            return self.remove_stale_container(&name).await;
        };
        let spec = self.node_spec(node);
        let current = self.named_container(&name).await?;
        let matches = current.as_ref().is_some_and(|current| {
            current["Config"]["Labels"][CONFIGURATION_LABEL] == spec["Labels"][CONFIGURATION_LABEL]
        });
        if let Some(current) = &current
            && matches
        {
            Self::check_container_configuration(current, &spec)?;
            if current["State"]["Running"] == true {
                return Ok(());
            }
        }
        self.prepare_node(node).await?;
        let journal = self.journal("ingress_start_tailnet", &name).await?;
        let result = async {
            if !matches {
                // Node state is a host directory, so a replacement keeps its identity.
                self.remove_container(&journal, &name).await?;
                self.create_container(&journal, &name, &spec).await?;
            }
            self.docker
                .send(
                    &journal,
                    Method::POST,
                    &format!("/containers/{name}/start"),
                    None,
                )
                .await?;
            tracing::info!(node=%name, hostname=%node.hostname, image=TAILSCALE_IMAGE, "started apps tailnet node");
            Ok(())
        }
        .await;
        journal.finish(result).await
    }

    /// Prepares the node's directories, serve configuration, auth key, and image.
    async fn prepare_node(&self, node: &Node) -> Result<()> {
        let directory = self.node_directory();
        for name in ["", "state", "run", "config"] {
            crate::prepare_data_dir(&directory.join(name)).await?;
        }
        let config = directory.join("config");
        super::certificates::write_private(
            &config.join("serve.json"),
            &serde_json::to_vec(&self.serve_configuration())?,
        )
        .await?;
        let key = config.join("auth-key");
        match &node.auth_key {
            Some(key_value) => {
                super::certificates::write_private(&key, key_value.expose().as_bytes()).await?;
            }
            None => match tokio::fs::remove_file(&key).await {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(error).context("remove the unconfigured auth key");
                }
                _ => {}
            },
        }
        self.ensure_image(TAILSCALE_IMAGE).await
    }

    /// The node's login state and tailnet addresses from its `LocalAPI`. Relays
    /// a new login URL to daemon logs once.
    pub(super) async fn node_status(&self, node: &Node) -> Result<PrivateIngressStatus> {
        let status: LocalStatus = serde_json::from_value(
            node.api
                .get("/localapi/v0/status")
                .await
                .context("read the apps node status")?,
        )
        .context("decode the apps node status")?;
        let login_url = Some(status.auth_url.clone()).filter(|url| !url.is_empty());
        let mut relayed = node.login_url.lock().expect("login URL lock");
        if login_url.is_some() && *relayed != login_url {
            tracing::warn!(node=%node.hostname, url=%status.auth_url,
                "the apps tailnet node needs login; open the URL, or set [ingress.private] auth_key_file");
        }
        *relayed = login_url;
        let running = status.backend_state == "Running";
        Ok(PrivateIngressStatus {
            enabled: true,
            healthy: running,
            message: if running {
                "The apps node is on the tailnet and forwards to the private listener".into()
            } else if status.backend_state == "NeedsLogin" {
                "The apps node needs login: open the login URL from the daemon logs, or set [ingress.private] auth_key_file".into()
            } else {
                format!("The apps node is {}", status.backend_state)
            },
            state: status.backend_state,
            dns_name: status
                .node
                .as_ref()
                .map(|node| node.dns_name.trim_end_matches('.').to_owned())
                .filter(|name| !name.is_empty()),
            addresses: status
                .node
                .into_iter()
                .flat_map(|node| node.addresses.unwrap_or_default())
                .map(|address| address.to_string())
                .collect(),
        })
    }
}
