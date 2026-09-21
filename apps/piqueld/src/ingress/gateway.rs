use super::{CADDY_IMAGE, Ingress};
use crate::store::ingress::RoutingTable;
use anyhow::{Context, Result, ensure};
use futures_util::TryStreamExt;
use hyper::Method;
use piqueld_core::{
    DockerNetworkName,
    resource::{APPLICATION_LABEL, INSTANCE_LABEL, MANAGED_LABEL},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, time::Duration};

const GATEWAY_LABEL: &str = "io.piqueld.ingress";
const CONFIGURATION_LABEL: &str = "io.piqueld.ingress-configuration";

impl Ingress {
    fn labels(&self) -> Value {
        json!({MANAGED_LABEL:"true",INSTANCE_LABEL:self.instance_id,GATEWAY_LABEL:"true"})
    }

    fn check_owner(&self, labels: &Value) -> Result<()> {
        ensure!(
            labels[MANAGED_LABEL] == "true"
                && labels[INSTANCE_LABEL] == self.instance_id
                && labels[GATEWAY_LABEL] == "true",
            "gateway resource is not owned by this installation"
        );
        Ok(())
    }

    pub(super) async fn container(&self) -> Result<Option<Value>> {
        let container = self
            .docker
            .inspect(&format!("/containers/{}/json", self.name))
            .await?;
        if let Some(container) = &container {
            self.check_owner(&container["Config"]["Labels"])?;
        }
        Ok(container)
    }

    pub(super) async fn stop_gateway(&self) -> Result<()> {
        if let Some(container) = self.container().await? {
            // Removing the container also removes its restart policy. Persistent
            // data and config are host directories retained across disablement.
            self.docker
                .json(
                    Method::DELETE,
                    &format!(
                        "/containers/{}?force=true",
                        container["Id"].as_str().context("container ID missing")?
                    ),
                    None,
                )
                .await?;
            tracing::info!(gateway=%self.name,"stopped managed ingress");
        }
        Ok(())
    }

    async fn check_version(&self) -> Result<()> {
        let version = self.docker.json(Method::GET, "/version", None).await?;
        let major = version["Version"]
            .as_str()
            .and_then(|v| v.split('.').next())
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        let api = version["ApiVersion"]
            .as_str()
            .and_then(|v| v.split_once('.'))
            .and_then(|(major, minor)| {
                Some((major.parse::<u32>().ok()?, minor.parse::<u32>().ok()?))
            });
        ensure!(
            major >= 28 && api.is_some_and(|v| v >= (1, 48)),
            "Docker Engine 28+ with API 1.48+ is required for ingress network gateway priority"
        );
        Ok(())
    }

    async fn ensure_edge_network(&self) -> Result<()> {
        if let Some(network) = self
            .docker
            .inspect(&format!("/networks/{}", self.name))
            .await?
        {
            self.check_owner(&network["Labels"])?;
            ensure!(
                network["Driver"] == "bridge" && network["Internal"] == false,
                "gateway egress network configuration conflicts"
            );
        } else {
            self.docker
                .json(
                    Method::POST,
                    "/networks/create",
                    Some(&json!({"Name":self.name,"Driver":"bridge","Labels":self.labels()})),
                )
                .await?;
        }
        Ok(())
    }

    async fn ingress_networks(&self, table: &RoutingTable) -> Result<BTreeSet<String>> {
        let mut networks = BTreeSet::new();
        for (id, routes) in table {
            if routes.is_empty() {
                continue;
            }
            let name = DockerNetworkName::for_ingress(id).to_string();
            let network = self
                .docker
                .inspect(&format!("/networks/{name}"))
                .await?
                .context("application ingress network is not ready")?;
            ensure!(
                network["Labels"][MANAGED_LABEL] == "true"
                    && network["Labels"][INSTANCE_LABEL] == self.instance_id
                    && network["Labels"][APPLICATION_LABEL] == id.as_str(),
                "application ingress network has conflicting ownership"
            );
            ensure!(
                network["Driver"] == "overlay" && network["Attachable"] == true,
                "application ingress network must be an attachable overlay"
            );
            networks.insert(name);
        }
        Ok(networks)
    }

    fn container_spec(&self) -> Value {
        let uid = rustix::process::geteuid().as_raw();
        let gid = rustix::process::getegid().as_raw();
        let binds: Vec<_> = ["data", "config", "control"]
            .into_iter()
            .map(|name| format!("{}:/{name}", self.directory.join(name).display()))
            .collect();
        let mut spec = json!({
            "Image":CADDY_IMAGE,"User":format!("{uid}:{gid}"),
            "Cmd":["caddy","run","--resume","--config","/config/caddy/autosave.json"],
            "Env":["XDG_DATA_HOME=/data","XDG_CONFIG_HOME=/config"],
            "Labels":self.labels(),
            "ExposedPorts":{"80/tcp":{},"443/tcp":{}},
            "HostConfig":{
                "Binds":binds,"RestartPolicy":{"Name":"unless-stopped"},
                "PortBindings":{"80/tcp":[{"HostIp":"","HostPort":"80"}],"443/tcp":[{"HostIp":"","HostPort":"443"}]},
                "ReadonlyRootfs":true,"CapDrop":["ALL"],"CapAdd":["NET_BIND_SERVICE"],"SecurityOpt":["no-new-privileges:true"],
                "Sysctls":{"net.ipv4.ip_unprivileged_port_start":"0"},
                "Tmpfs":{"/tmp":"rw,noexec,nosuid,size=16777216"},
                "LogConfig":{"Type":"local","Config":{"max-size":"10m","max-file":"3"}},
                "NetworkMode":self.name
            },
            "NetworkingConfig":{"EndpointsConfig":{&self.name:{"GwPriority":1}}}
        });
        let hash = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&spec).expect("JSON serializes"))
        );
        spec["Labels"][CONFIGURATION_LABEL] = hash.into();
        spec
    }

    pub(super) async fn ensure_gateway(&self, table: &RoutingTable) -> Result<()> {
        self.check_version().await?;
        let spec = self.container_spec();
        let current = self.container().await?;
        if let Some(current) = &current {
            if current["Config"]["Labels"][CONFIGURATION_LABEL]
                == spec["Labels"][CONFIGURATION_LABEL]
            {
                Self::check_container_configuration(current, &spec)?;
                if current["State"]["Running"] == true {
                    return Ok(());
                }
            } else {
                self.stop_gateway().await?;
            }
        }
        for directory in [
            &self.directory,
            self.directory.join("data").as_path(),
            self.directory.join("config").as_path(),
            self.directory.join("control").as_path(),
            self.directory.join("config/caddy").as_path(),
        ] {
            crate::prepare_data_dir(directory).await?;
        }
        self.ensure_edge_network().await?;
        let networks = self.ingress_networks(table).await?;
        if self
            .docker
            .inspect(&format!("/images/{CADDY_IMAGE}/json"))
            .await?
            .is_none()
        {
            use bollard::query_parameters::CreateImageOptionsBuilder;
            tokio::time::timeout(Duration::from_secs(180), async {
                let mut pull = self.images.create_image(
                    Some(
                        CreateImageOptionsBuilder::default()
                            .from_image(CADDY_IMAGE)
                            .build(),
                    ),
                    None,
                    None,
                );
                while let Some(event) = pull.try_next().await? {
                    if let Some(error) = event.error {
                        anyhow::bail!("pull Caddy image: {error}");
                    }
                }
                Ok::<_, anyhow::Error>(())
            })
            .await
            .context("pull Caddy image timed out")??;
        }
        if self.container().await?.is_none() {
            self.docker
                .json(
                    Method::POST,
                    &format!("/containers/create?name={}", self.name),
                    Some(&spec),
                )
                .await?;
        }
        self.attach_networks(&networks).await?;
        self.start_gateway(table).await
    }

    async fn start_gateway(&self, table: &RoutingTable) -> Result<()> {
        // A restarted gateway must never briefly resume routes that were removed
        // by deployments while ingress was disabled.
        let path = self.directory.join("config/caddy/autosave.json");
        let temporary = path.with_extension("tmp");
        tokio::fs::write(&temporary, serde_json::to_vec(&self.configuration(table))?).await?;
        tokio::fs::rename(&temporary, &path).await?;
        self.docker
            .json(
                Method::POST,
                &format!("/containers/{}/start", self.name),
                None,
            )
            .await?;
        // Docker reports a running process before Caddy opens its admin socket.
        // Wait for startup here instead of failing an otherwise healthy deployment.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match self.caddy.json(Method::GET, "/config/", None).await {
                    Ok(_) => return,
                    Err(error) => tracing::debug!(?error, "waiting for Caddy administration"),
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .context("Caddy administration did not become ready after startup; inspect gateway logs")?;
        tracing::info!(gateway=%self.name,image=CADDY_IMAGE,"started managed ingress");
        Ok(())
    }

    fn check_container_configuration(current: &Value, spec: &Value) -> Result<()> {
        for field in ["Image", "User", "Cmd"] {
            ensure!(
                current["Config"][field] == spec[field],
                "gateway container {field} conflicts with managed configuration"
            );
        }
        for field in [
            "Binds",
            "RestartPolicy",
            "ReadonlyRootfs",
            "CapDrop",
            "CapAdd",
            "SecurityOpt",
            "Sysctls",
            "Tmpfs",
            "NetworkMode",
        ] {
            // Docker fills MaximumRetryCount in a restart policy.
            if field == "RestartPolicy" {
                ensure!(
                    current["HostConfig"][field]["Name"] == "unless-stopped",
                    "gateway restart policy conflicts"
                );
            } else {
                ensure!(
                    current["HostConfig"][field] == spec["HostConfig"][field],
                    "gateway host configuration {field} conflicts"
                );
            }
        }
        ensure!(
            current["HostConfig"]["PortBindings"] == spec["HostConfig"]["PortBindings"],
            "gateway public port bindings conflict"
        );
        let environment = current["Config"]["Env"]
            .as_array()
            .context("gateway environment is absent")?;
        for expected in spec["Env"]
            .as_array()
            .context("managed environment is absent")?
        {
            ensure!(
                environment.contains(expected),
                "gateway storage environment conflicts"
            );
        }
        ensure!(
            current["HostConfig"]["Privileged"] != true,
            "gateway must not be privileged"
        );
        Ok(())
    }

    async fn attach_networks(&self, desired: &BTreeSet<String>) -> Result<()> {
        let container = self
            .container()
            .await?
            .context("gateway container disappeared")?;
        for network in desired {
            if container["NetworkSettings"]["Networks"]
                .get(network)
                .is_none()
            {
                self.docker
                    .json(
                        Method::POST,
                        &format!("/networks/{network}/connect"),
                        Some(&json!({"Container":self.name,"EndpointConfig":{"GwPriority":0}})),
                    )
                    .await?;
            }
        }
        Ok(())
    }

    pub(super) async fn configure_gateway(&self, table: &RoutingTable) -> Result<()> {
        let networks = self.ingress_networks(table).await?;
        self.attach_networks(&networks).await?;
        let desired = self.configuration(table);
        let current = self.caddy.json(Method::GET, "/config/", None).await?;
        if current != desired {
            self.caddy
                .json(Method::POST, "/load", Some(&desired))
                .await?;
            tracing::info!(
                applications = table.len(),
                "applied ingress routing configuration"
            );
        }
        let container = self
            .container()
            .await?
            .context("gateway container disappeared")?;
        if let Some(attached) = container["NetworkSettings"]["Networks"].as_object() {
            for network in attached
                .keys()
                .filter(|name| *name != &self.name && !networks.contains(*name))
            {
                self.docker
                    .json(
                        Method::POST,
                        &format!("/networks/{network}/disconnect"),
                        Some(&json!({"Container":self.name,"Force":false})),
                    )
                    .await?;
            }
        }
        Ok(())
    }

    pub(super) async fn relay_logs(&self) -> Result<()> {
        if !self.enabled || self.container().await?.is_none() {
            return Ok(());
        }
        let mut since = self.logs_since.lock().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let path = format!(
            "/containers/{}/logs?stdout=1&stderr=1&tail=100&since={}&until={now}",
            self.name, *since
        );
        let (status, bytes) = self.docker.request(Method::GET, &path, None).await?;
        ensure!(status.is_success(), "Caddy diagnostics returned {status}");
        let mut remaining = bytes.as_slice();
        while remaining.len() >= 8 {
            let length = u32::from_be_bytes(remaining[4..8].try_into()?) as usize;
            ensure!(length <= remaining.len() - 8, "truncated Caddy log frame");
            for line in String::from_utf8_lossy(&remaining[8..8 + length]).lines() {
                tracing::info!(gateway=%self.name,caddy=%line.chars().take(4096).collect::<String>(),"Caddy diagnostic");
            }
            remaining = &remaining[8 + length..];
        }
        *since = now;
        Ok(())
    }
}
