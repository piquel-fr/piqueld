//! Manages this installation's Caddy gateway container in Docker: creating and
//! starting it, connecting application networks, and loading the routes built by
//! `configuration`. Replacements keep the old container and configuration for
//! recovery if startup fails. Disabling ingress removes the managed containers
//! but keeps certificates and configuration on disk.
//!
//! Each change is one daemon-scoped journal action (`ingress_*` phases). Methods
//! decide from reads before opening an action, so steady-state passes record no
//! history. Helpers taking a [`Journaled`] run inside their caller's action.

use super::{CADDY_IMAGE, Ingress, wire::Journaled};
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
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

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
        self.named_container(&self.name).await
    }

    async fn named_container(&self, name: &str) -> Result<Option<Value>> {
        let container = self
            .docker
            .inspect(&format!("/containers/{name}/json"))
            .await?;
        if let Some(container) = &container {
            self.check_owner(&container["Config"]["Labels"])?;
        }
        Ok(container)
    }

    pub(super) async fn stop_gateway(&self) -> Result<()> {
        let mut existing = Vec::new();
        for name in [
            self.name.clone(),
            format!("{}-previous", self.name),
            format!("{}-next", self.name),
        ] {
            if self.named_container(&name).await?.is_some() {
                existing.push(name);
            }
        }
        if existing.is_empty() {
            return Ok(());
        }
        let journal = self.journal("ingress_stop_gateway", &self.name).await?;
        let result = async {
            for name in &existing {
                self.remove_container(&journal, name).await?;
            }
            Ok(())
        }
        .await;
        journal.finish(result).await
    }

    /// Removes a leftover container in its own action; used for best-effort cleanup.
    async fn remove_stale_container(&self, name: &str) -> Result<()> {
        if self.named_container(name).await?.is_none() {
            return Ok(());
        }
        let journal = self.journal("ingress_remove_container", name).await?;
        let result = self.remove_container(&journal, name).await;
        journal.finish(result).await
    }

    async fn remove_container(&self, journal: &Journaled<'_>, name: &str) -> Result<()> {
        if let Some(container) = self.named_container(name).await? {
            // Removing the container also removes its restart policy. Persistent
            // data and config are host directories retained across disablement.
            self.docker
                .send(
                    journal,
                    Method::DELETE,
                    &format!(
                        "/containers/{}?force=true",
                        container["Id"].as_str().context("container ID missing")?
                    ),
                    None,
                )
                .await?;
            tracing::info!(container=%name,"removed managed ingress container");
        }
        Ok(())
    }

    async fn check_version(&self) -> Result<()> {
        let version = self.docker.get("/version").await?;
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
            let journal = self.journal("ingress_create_network", &self.name).await?;
            let result = self
                .docker
                .send(
                    &journal,
                    Method::POST,
                    "/networks/create",
                    Some(&json!({"Name":self.name,"Driver":"bridge","Labels":self.labels()})),
                )
                .await;
            journal.finish(result).await?;
        }
        Ok(())
    }

    /// Keep unavailable apps on their accepted destinations, but always honor
    /// withdrawals. Never attach an unverified network or acknowledge a new route
    /// for an app whose network failed validation.
    pub(super) async fn prepare_routes(
        &self,
        desired: &RoutingTable,
    ) -> Result<(
        RoutingTable,
        BTreeSet<String>,
        BTreeMap<piqueld_core::ApplicationId, String>,
    )> {
        let mut table = desired.clone();
        let mut networks = BTreeSet::new();
        let mut failures = BTreeMap::new();
        for (id, routes) in &mut table {
            if routes.is_empty() {
                continue;
            }
            let name = DockerNetworkName::for_ingress(id).to_string();
            match self.check_ingress_network(id, &name).await {
                Ok(()) => {
                    networks.insert(name);
                }
                Err(error) => {
                    tracing::error!(application_id=%id, network=%name, error=?error,
                        "preserving accepted destinations; other applications can still update");
                    let mut accepted = self.store.applied_routes(id).await?;
                    accepted.retain(|old| routes.iter().any(|new| new.hostname == old.hostname));
                    *routes = accepted;
                    failures.insert(id.clone(), format!("{error:#}"));
                }
            }
        }
        Ok((table, networks, failures))
    }

    async fn check_ingress_network(
        &self,
        id: &piqueld_core::ApplicationId,
        name: &str,
    ) -> Result<()> {
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
        Ok(())
    }

    pub(super) fn container_spec(&self) -> Value {
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
        #[cfg(test)]
        if !self.extra_hosts.is_empty() {
            spec["HostConfig"]["ExtraHosts"] = json!(self.extra_hosts);
        }
        let hash = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&spec).expect("JSON serializes"))
        );
        spec["Labels"][CONFIGURATION_LABEL] = hash.into();
        spec
    }

    pub(super) async fn ensure_gateway(
        &self,
        table: &RoutingTable,
        networks: &BTreeSet<String>,
    ) -> Result<()> {
        self.check_version().await?;
        self.recover_gateway().await?;
        let spec = self.container_spec();
        let current = self.container().await?;
        if let Some(current) = &current
            && current["Config"]["Labels"][CONFIGURATION_LABEL]
                == spec["Labels"][CONFIGURATION_LABEL]
        {
            Self::check_container_configuration(current, &spec)?;
            if current["State"]["Running"] == true {
                return Ok(());
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
        if self
            .docker
            .inspect(&format!("/images/{CADDY_IMAGE}/json"))
            .await?
            .is_none()
        {
            self.pull_image().await?;
        }
        if current.as_ref().is_some_and(|container| {
            container["Config"]["Labels"][CONFIGURATION_LABEL]
                != spec["Labels"][CONFIGURATION_LABEL]
        }) {
            return self.replace_gateway(table, networks, &spec).await;
        }
        let journal = self.journal("ingress_start_gateway", &self.name).await?;
        let result = async {
            if current.is_none() {
                self.create_container(&journal, &self.name, &spec).await?;
            }
            self.attach_networks(&journal, &self.name, networks).await?;
            self.start_gateway(&journal, table).await
        }
        .await;
        journal.finish(result).await
    }

    async fn pull_image(&self) -> Result<()> {
        use bollard::query_parameters::CreateImageOptionsBuilder;
        let journal = self.journal("ingress_pull_image", CADDY_IMAGE).await?;
        let result = async {
            journal.request().await?;
            tokio::time::timeout(Duration::from_mins(3), async {
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
            .context("pull Caddy image timed out")?
        }
        .await;
        journal.finish(result).await
    }

    async fn create_container(
        &self,
        journal: &Journaled<'_>,
        name: &str,
        spec: &Value,
    ) -> Result<()> {
        self.docker
            .send(
                journal,
                Method::POST,
                &format!("/containers/create?name={name}"),
                Some(spec),
            )
            .await?;
        Ok(())
    }

    /// Both containers and the rollback configuration survive cancellation or a
    /// daemon crash. Reconciliation restores the old gateway if cutover did not
    /// finish; disablement removes all three managed container names. Discarding
    /// the rollback configuration commits a started replacement, so failing to
    /// clean up the old container never reverts a serving gateway.
    /// Defers replacement while retained routes lack verified networks, allowing
    /// the running gateway to keep its attachments and accept other route changes.
    pub(super) async fn replace_gateway(
        &self,
        table: &RoutingTable,
        networks: &BTreeSet<String>,
        spec: &Value,
    ) -> Result<()> {
        if let Some((id, _)) = table.iter().find(|(id, routes)| {
            !routes.is_empty()
                && !networks.contains(&DockerNetworkName::for_ingress(id).to_string())
        }) {
            ensure!(
                self.container()
                    .await?
                    .is_some_and(|container| container["State"]["Running"] == true),
                "cannot replace gateway: retained routes for application {id} lack a verified network and the old gateway is not running"
            );
            tracing::warn!(application_id=%id,
                "deferring gateway replacement until retained route networks are repaired or routes are withdrawn");
            return Ok(());
        }
        let journal = self.journal("ingress_replace_gateway", &self.name).await?;
        let result = self.cut_over(&journal, table, networks, spec).await;
        journal.finish(result).await?;
        if let Err(error) = self
            .remove_stale_container(&format!("{}-previous", self.name))
            .await
        {
            tracing::warn!(gateway=%self.name, error=%format!("{error:#}"),
                "replaced gateway could not be removed; recovery retries the cleanup");
        }
        Ok(())
    }

    async fn cut_over(
        &self,
        journal: &Journaled<'_>,
        table: &RoutingTable,
        networks: &BTreeSet<String>,
        spec: &Value,
    ) -> Result<()> {
        let next = format!("{}-next", self.name);
        let previous = format!("{}-previous", self.name);
        self.validate_replacement(journal, &next, spec, table)
            .await?;
        self.create_container(journal, &next, spec).await?;
        self.attach_networks(journal, &next, networks).await?;
        let configuration = if self
            .container()
            .await?
            .is_some_and(|container| container["State"]["Running"] == true)
        {
            self.caddy.get("/config/").await?
        } else {
            let bytes = tokio::fs::read(self.directory.join("config/caddy/autosave.json")).await?;
            serde_json::from_slice(&bytes)?
        };
        self.write_configuration("rollback.json", &configuration)
            .await?;
        // All preparation above leaves the current listener untouched.
        let result = async {
            self.docker
                .send(
                    journal,
                    Method::POST,
                    &format!("/containers/{}/stop?t=10", self.name),
                    None,
                )
                .await?;
            // Docker cannot reliably rename a running overlay endpoint.
            self.rename_container(journal, &self.name, &previous)
                .await?;
            self.rename_container(journal, &next, &self.name).await?;
            self.start_gateway(journal, table).await
        }
        .await;
        if let Err(error) = result {
            self.restore_gateway(journal).await.with_context(|| {
                format!("gateway replacement failed ({error:#}); rollback also failed")
            })?;
            return Err(error.context("gateway replacement failed; previous gateway restored"));
        }
        tokio::fs::remove_file(self.rollback_path()).await?;
        Ok(())
    }

    async fn validate_replacement(
        &self,
        journal: &Journaled<'_>,
        name: &str,
        spec: &Value,
        table: &RoutingTable,
    ) -> Result<()> {
        self.write_configuration("candidate.json", &self.configuration(table))
            .await?;
        let mut validation = spec.clone();
        validation["Cmd"] = json!([
            "caddy",
            "validate",
            "--config",
            "/config/caddy/candidate.json"
        ]);
        validation["HostConfig"]["PortBindings"] = json!({});
        validation["HostConfig"]["RestartPolicy"] = json!({"Name":"no"});
        self.remove_container(journal, name).await?;
        self.create_container(journal, name, &validation).await?;
        self.docker
            .send(
                journal,
                Method::POST,
                &format!("/containers/{name}/start"),
                None,
            )
            .await?;
        let exited = self
            .docker
            .send(
                journal,
                Method::POST,
                &format!("/containers/{name}/wait?condition=not-running"),
                None,
            )
            .await?;
        if exited["StatusCode"] != 0 {
            let diagnostics = self.container_logs(name, "tail=20").await.context(
                "replacement Caddy configuration validation failed; could not read diagnostics",
            )?;
            anyhow::bail!(
                "replacement Caddy configuration validation failed: {}",
                diagnostics.join("\n")
            );
        }
        self.remove_container(journal, name).await
    }

    async fn rename_container(&self, journal: &Journaled<'_>, from: &str, to: &str) -> Result<()> {
        self.docker
            .send(
                journal,
                Method::POST,
                &format!("/containers/{from}/rename?name={to}"),
                None,
            )
            .await?;
        Ok(())
    }

    /// Finishes or reverts a replacement interrupted by cancellation or a crash.
    pub(super) async fn recover_gateway(&self) -> Result<()> {
        let previous = format!("{}-previous", self.name);
        if self.named_container(&previous).await?.is_some()
            && !tokio::fs::try_exists(self.rollback_path()).await?
        {
            // The replacement committed; only its cleanup was interrupted.
            return self.remove_stale_container(&previous).await;
        }
        let next = format!("{}-next", self.name);
        if self.named_container(&previous).await?.is_none()
            && self.named_container(&next).await?.is_none()
        {
            return Ok(());
        }
        let journal = self.journal("ingress_recover_gateway", &self.name).await?;
        let result = self.restore_gateway(&journal).await;
        journal.finish(result).await
    }

    /// Restores the gateway that an unfinished replacement stopped.
    async fn restore_gateway(&self, journal: &Journaled<'_>) -> Result<()> {
        let previous = format!("{}-previous", self.name);
        let next = format!("{}-next", self.name);
        if self.named_container(&previous).await?.is_some() {
            self.remove_container(journal, &self.name).await?;
            self.restore_configuration().await?;
            self.rename_container(journal, &previous, &self.name)
                .await?;
            self.start_container(journal).await?;
            self.remove_container(journal, &next).await?;
            tracing::warn!(gateway=%self.name, "restored gateway after interrupted replacement");
        } else if self.named_container(&next).await?.is_some() {
            // Cancellation may land between stopping the old container and
            // renaming it. Keep the candidate until the old listener is restored.
            if let Some(container) = self.container().await?
                && container["State"]["Running"] != true
            {
                // Before the first rename the old autosave is still untouched.
                self.start_container(journal).await?;
            }
            self.remove_container(journal, &next).await?;
        }
        Ok(())
    }

    fn rollback_path(&self) -> std::path::PathBuf {
        self.directory.join("config/caddy/rollback.json")
    }

    async fn restore_configuration(&self) -> Result<()> {
        let bytes = tokio::fs::read(self.rollback_path()).await?;
        self.write_configuration("autosave.json", &serde_json::from_slice(&bytes)?)
            .await
    }

    async fn write_configuration(&self, file: &str, configuration: &Value) -> Result<()> {
        let path = self.directory.join("config/caddy").join(file);
        let temporary = path.with_extension("tmp");
        tokio::fs::write(&temporary, serde_json::to_vec(configuration)?).await?;
        tokio::fs::rename(&temporary, &path).await?;
        Ok(())
    }

    async fn start_gateway(&self, journal: &Journaled<'_>, table: &RoutingTable) -> Result<()> {
        // A restarted gateway must never briefly resume routes that were removed
        // by deployments while ingress was disabled.
        self.write_configuration("autosave.json", &self.configuration(table))
            .await?;
        self.start_container(journal).await
    }

    async fn start_container(&self, journal: &Journaled<'_>) -> Result<()> {
        self.docker
            .send(
                journal,
                Method::POST,
                &format!("/containers/{}/start", self.name),
                None,
            )
            .await?;
        // Docker reports a running process before Caddy opens its admin socket.
        // Wait for startup here instead of failing an otherwise healthy deployment.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match self.caddy.get("/config/").await {
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

    async fn attach_networks(
        &self,
        journal: &Journaled<'_>,
        name: &str,
        desired: &BTreeSet<String>,
    ) -> Result<()> {
        let container = self
            .named_container(name)
            .await?
            .context("gateway container disappeared")?;
        for network in Self::missing_networks(&container, desired) {
            self.docker
                .send(
                    journal,
                    Method::POST,
                    &format!("/networks/{network}/connect"),
                    Some(&json!({"Container":name,"EndpointConfig":{"GwPriority":0}})),
                )
                .await?;
        }
        Ok(())
    }

    fn missing_networks<'a>(
        container: &Value,
        desired: &'a BTreeSet<String>,
    ) -> impl Iterator<Item = &'a String> {
        let attached = container["NetworkSettings"]["Networks"].clone();
        desired
            .iter()
            .filter(move |network| attached.get(network.as_str()).is_none())
    }

    /// Converges attachments and routes; a matching gateway records no action.
    pub(super) async fn configure_gateway(
        &self,
        table: &RoutingTable,
        networks: &BTreeSet<String>,
    ) -> Result<()> {
        let container = self
            .container()
            .await?
            .context("gateway container disappeared")?;
        let desired = self.configuration(table);
        let reload = self.caddy.get("/config/").await? != desired;
        // Keep existing attachments for unavailable apps whose routes were retained.
        let retained: BTreeSet<_> = table
            .iter()
            .filter(|(_, routes)| !routes.is_empty())
            .map(|(id, _)| DockerNetworkName::for_ingress(id).to_string())
            .collect();
        let stale: Vec<String> = container["NetworkSettings"]["Networks"]
            .as_object()
            .into_iter()
            .flat_map(|attached| attached.keys())
            .filter(|name| *name != &self.name && !retained.contains(*name))
            .cloned()
            .collect();
        if !reload
            && stale.is_empty()
            && Self::missing_networks(&container, networks)
                .next()
                .is_none()
        {
            return Ok(());
        }
        let journal = self.journal("ingress_configure_routes", &self.name).await?;
        let result = async {
            self.attach_networks(&journal, &self.name, networks).await?;
            if reload {
                self.caddy
                    .send(&journal, Method::POST, "/load", Some(&desired))
                    .await?;
                tracing::info!(
                    applications = table.len(),
                    "applied ingress routing configuration"
                );
            }
            for network in &stale {
                self.docker
                    .send(
                        &journal,
                        Method::POST,
                        &format!("/networks/{network}/disconnect"),
                        Some(&json!({"Container":self.name,"Force":false})),
                    )
                    .await?;
            }
            Ok(())
        }
        .await;
        journal.finish(result).await
    }

    pub(super) async fn relay_logs(&self) -> Result<()> {
        if !self.enabled || self.container().await?.is_none() {
            return Ok(());
        }
        let mut since = self.logs_since.lock().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        for line in self
            .container_logs(
                &self.name,
                &format!("tail=100&since={}&until={now}", *since),
            )
            .await?
        {
            tracing::info!(gateway=%self.name,caddy=%line,"Caddy diagnostic");
        }
        *since = now;
        Ok(())
    }

    async fn container_logs(&self, name: &str, query: &str) -> Result<Vec<String>> {
        let path = format!("/containers/{name}/logs?stdout=1&stderr=1&{query}");
        let bytes = self.docker.read(&path).await?;
        let mut lines = Vec::new();
        let mut remaining = bytes.as_slice();
        while remaining.len() >= 8 {
            let length = u32::from_be_bytes(remaining[4..8].try_into()?) as usize;
            ensure!(length <= remaining.len() - 8, "truncated Caddy log frame");
            for line in String::from_utf8_lossy(&remaining[8..8 + length]).lines() {
                lines.push(line.chars().take(4096).collect());
            }
            remaining = &remaining[8 + length..];
        }
        Ok(lines)
    }
}
