//! Manages this installation's Caddy gateway container in Docker: creating and
//! starting it, connecting application networks, and loading the routes built by
//! `configuration`. Replacements keep the old container and configuration for
//! recovery if startup fails. Disabling ingress removes the managed containers,
//! including the apps tailnet node (`node`), but keeps certificates,
//! configuration and node state on disk.
//!
//! Each change is one daemon-scoped journal action (`ingress_*` phases). Methods
//! decide from reads before opening an action, so steady-state passes record no
//! history. Helpers taking a [`Journaled`] run inside their caller's action.

use super::{
    CADDY_IMAGE, Ingress,
    node::{PRIVATE_HTTPS_PORT, Subnet, TailnetOverlap},
    wire::Journaled,
};
use crate::store::ingress::RoutingTable;
use anyhow::{Context, Result, ensure};
use futures_util::TryStreamExt;
use hyper::Method;
use piqueld_core::{
    DockerNetworkName,
    manifest::ValidatedRoute,
    resource::{APPLICATION_LABEL, INSTANCE_LABEL, MANAGED_LABEL},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

/// Marks the gateway container, the apps node, and their egress network.
const GATEWAY_LABEL: &str = "io.piqueld.ingress";
/// SHA-256 of the managed container spec; a mismatch triggers replacement.
pub(super) const CONFIGURATION_LABEL: &str = "io.piqueld.ingress-configuration";

impl Ingress {
    /// Ownership labels applied to the gateway container, the apps node, and
    /// the egress network.
    pub(super) fn labels(&self) -> Value {
        json!({MANAGED_LABEL:"true",INSTANCE_LABEL:self.instance_id,GATEWAY_LABEL:"true"})
    }

    /// Rejects resources that exist under a managed name but were not created
    /// by this installation's gateway.
    fn check_owner(&self, labels: &Value) -> Result<()> {
        ensure!(
            labels[MANAGED_LABEL] == "true"
                && labels[INSTANCE_LABEL] == self.instance_id
                && labels[GATEWAY_LABEL] == "true",
            "gateway resource is not owned by this installation"
        );
        Ok(())
    }

    /// Inspects the serving gateway container, if it exists.
    pub(super) async fn container(&self) -> Result<Option<Value>> {
        self.named_container(&self.name).await
    }

    /// Inspects a container by name, verifying ownership when it exists.
    pub(super) async fn named_container(&self, name: &str) -> Result<Option<Value>> {
        let container = self
            .docker
            .inspect(&format!("/containers/{name}/json"))
            .await?;
        if let Some(container) = &container {
            self.check_owner(&container["Config"]["Labels"])?;
        }
        Ok(container)
    }

    /// Removes the serving, `-previous`, and `-next` gateway containers and the
    /// apps node in a single action when ingress is disabled. Every removal is
    /// attempted, so a failing one never keeps another serving; the first
    /// failure is returned. Records nothing if none exist.
    pub(super) async fn stop_gateway(&self) -> Result<()> {
        let mut existing = Vec::new();
        let mut failure = None;
        for name in [
            self.node_name(),
            self.name.clone(),
            format!("{}-previous", self.name),
            format!("{}-next", self.name),
        ] {
            match self.named_container(&name).await {
                Ok(Some(_)) => existing.push(name),
                Ok(None) => {}
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        if !existing.is_empty() {
            let journal = self.journal("ingress_stop_gateway", &self.name).await?;
            let mut result = Ok(());
            for name in &existing {
                if let Err(error) = self.remove_container(&journal, name).await {
                    result = result.and(Err(error));
                }
            }
            journal.finish(result).await?;
        }
        failure.map_or(Ok(()), Err)
    }

    /// Removes a leftover container in its own action; used for best-effort cleanup.
    pub(super) async fn remove_stale_container(&self, name: &str) -> Result<()> {
        if self.named_container(name).await?.is_none() {
            return Ok(());
        }
        let journal = self.journal("ingress_remove_container", name).await?;
        let result = self.remove_container(&journal, name).await;
        journal.finish(result).await
    }

    /// Force-removes a container by name if it exists.
    pub(super) async fn remove_container(&self, journal: &Journaled<'_>, name: &str) -> Result<()> {
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

    /// Requires Docker Engine 28+ with API 1.48+, which support the per-endpoint
    /// `GwPriority` used to keep the edge network as the default route.
    ///
    /// ```text
    /// {"Version": "28.1.1", "ApiVersion": "1.49"}  -> ok
    /// {"Version": "27.5.0", "ApiVersion": "1.47"}  -> error
    /// ```
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

    /// The subnets of the edge network: the only peers whose PROXY headers the
    /// private listener accepts. The apps node is one of them; application
    /// networks, also attached to the gateway, are not.
    pub(super) async fn edge_subnets(&self) -> Result<Vec<String>> {
        let network = self
            .docker
            .inspect(&format!("/networks/{}", self.name))
            .await?
            .context("gateway edge network is not ready")?;
        let subnets: Vec<String> = network["IPAM"]["Config"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|config| config["Subnet"].as_str().map(str::to_owned))
            .collect();
        ensure!(!subnets.is_empty(), "gateway edge network has no subnet");
        Ok(subnets)
    }

    /// Where the daemon reaches the private listener: the gateway's address on
    /// the edge network, which the host routes to.
    pub(super) async fn private_listener(&self) -> Result<std::net::SocketAddr> {
        #[cfg(test)]
        if let Some(port) = self.private_port {
            return Ok(std::net::SocketAddr::from(([127, 0, 0, 1], port)));
        }
        let container = self
            .container()
            .await?
            .context("gateway container is not running")?;
        let address: std::net::IpAddr =
            container["NetworkSettings"]["Networks"][&self.name]["IPAddress"]
                .as_str()
                .context("gateway has no edge network address")?
                .parse()
                .context("decode the gateway's edge network address")?;
        Ok(std::net::SocketAddr::new(address, PRIVATE_HTTPS_PORT))
    }

    /// Ensures the gateway's own non-internal bridge network (named like the
    /// container), which provides public egress for ACME and published ports.
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

    /// Whether an application's routes reach a backend through its ingress
    /// network. Redirect-only applications are answered by Caddy and have none.
    fn proxies(routes: &[ValidatedRoute]) -> bool {
        routes.iter().any(|route| route.target.service().is_some())
    }

    /// Splits the desired routing table into what can safely be applied now.
    ///
    /// Returns the table to apply, the verified ingress networks to attach, and
    /// per-application network failures. Applications without proxied routes
    /// (withdrawn or redirect-only) need no network and are passed through.
    /// Keep unavailable apps on their accepted destinations, but always honor
    /// withdrawals. Never attach an unverified network or acknowledge a new route
    /// for an app whose network failed validation. A network overlapping the
    /// tailnet ranges instead withdraws the app's proxied routes, which detaches
    /// it: its containers could otherwise pass for tailnet clients.
    pub(super) async fn prepare_routes(
        &self,
        desired: &RoutingTable,
    ) -> Result<(
        RoutingTable,
        BTreeSet<String>,
        BTreeMap<piqueld_core::EnvironmentId, String>,
    )> {
        let mut table = desired.clone();
        let mut networks = BTreeSet::new();
        let mut failures = BTreeMap::new();
        for (id, routes) in &mut table {
            if !Self::proxies(routes) {
                continue;
            }
            let name = DockerNetworkName::for_ingress(id).to_string();
            match self.check_ingress_network(id, &name).await {
                Ok(()) => {
                    networks.insert(name);
                }
                Err(error) if error.is::<TailnetOverlap>() => {
                    tracing::error!(environment_id=%id, network=%name, error=?error,
                        "withdrawing proxied routes; other applications can still update");
                    routes.retain(|route| route.target.service().is_none());
                    failures.insert(id.clone(), format!("{error:#}"));
                }
                Err(error) => {
                    tracing::error!(environment_id=%id, network=%name, error=?error,
                        "preserving accepted destinations; other applications can still update");
                    let mut accepted = self.store.applied_routes(id).await?;
                    accepted.retain(|old| routes.iter().any(|new| new.same_listener(old)));
                    *routes = accepted;
                    failures.insert(id.clone(), format!("{error:#}"));
                }
            }
        }
        Ok((table, networks, failures))
    }

    /// Verifies an application's ingress network exists, is owned by that
    /// application on this instance, and is an attachable overlay, outside the
    /// tailnet ranges while private ingress is enabled.
    async fn check_ingress_network(
        &self,
        id: &piqueld_core::EnvironmentId,
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
        if self.node.is_some() {
            for config in network["IPAM"]["Config"].as_array().into_iter().flatten() {
                let subnet = config["Subnet"]
                    .as_str()
                    .context("application ingress network has no subnet")?;
                let parsed = Subnet::parse(subnet)
                    .with_context(|| format!("decode ingress network subnet {subnet}"))?;
                if parsed.overlaps_tailnet() {
                    return Err(TailnetOverlap {
                        subnet: subnet.to_owned(),
                    }
                    .into());
                }
            }
        }
        Ok(())
    }

    /// Builds the hardened Caddy container spec: runs as the daemon's user with a
    /// read-only root, only `NET_BIND_SERVICE`, host-bound ports 80/443, and data,
    /// config, and control directories bind-mounted from the ingress directory.
    /// The private listener's ports are never published. The spec's own hash is
    /// stored in `CONFIGURATION_LABEL` to detect drift.
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
        // Tests outside the engine reach the private listener on a published port.
        #[cfg(test)]
        if self.private_port.is_some() {
            spec["ExposedPorts"]["8443/tcp"] = json!({});
            spec["HostConfig"]["PortBindings"]["8443/tcp"] =
                json!([{"HostIp":"","HostPort":"8443"}]);
        }
        Self::label_hash(&mut spec);
        spec
    }

    /// Stores the SHA-256 of a container spec in its `CONFIGURATION_LABEL`.
    pub(super) fn label_hash(spec: &mut Value) {
        let hash = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&spec).expect("JSON serializes"))
        );
        spec["Labels"][CONFIGURATION_LABEL] = hash.into();
    }

    /// Ensures a running gateway matching the current container spec.
    ///
    /// 1. Checks Docker compatibility and recovers any interrupted replacement.
    /// 2. Returns early when the container matches the spec hash and is running
    ///    (after verifying security-relevant settings were not altered).
    /// 3. Prepares host directories, the edge network, and the image.
    /// 4. Replaces a container with an outdated spec; otherwise creates (if
    ///    missing), attaches networks, and starts it with fresh configuration.
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
        self.ensure_image(CADDY_IMAGE).await?;
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

    /// Pulls a pinned image unless Docker already has it.
    pub(super) async fn ensure_image(&self, image: &str) -> Result<()> {
        if self
            .docker
            .inspect(&format!("/images/{image}/json"))
            .await?
            .is_none()
        {
            self.pull_image(image).await?;
        }
        Ok(())
    }

    /// Pulls a pinned image in its own action, bounded to three minutes.
    async fn pull_image(&self, image: &str) -> Result<()> {
        use bollard::query_parameters::CreateImageOptionsBuilder;
        let journal = self.journal("ingress_pull_image", image).await?;
        let result = async {
            journal.request().await?;
            tokio::time::timeout(Duration::from_mins(3), async {
                let mut pull = self.images.create_image(
                    Some(
                        CreateImageOptionsBuilder::default()
                            .from_image(image)
                            .build(),
                    ),
                    None,
                    None,
                );
                while let Some(event) = pull.try_next().await? {
                    if let Some(error) = event.error {
                        anyhow::bail!("pull {image}: {error}");
                    }
                }
                Ok::<_, anyhow::Error>(())
            })
            .await
            .with_context(|| format!("pull {image} timed out"))?
        }
        .await;
        journal.finish(result).await
    }

    /// Creates a container with the given name and spec without starting it.
    pub(super) async fn create_container(
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

    /// Replaces the gateway container with one built from `spec`, then removes
    /// the old container in a separate best-effort action.
    ///
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
            Self::proxies(routes)
                && !networks.contains(&DockerNetworkName::for_ingress(id).to_string())
        }) {
            ensure!(
                self.container()
                    .await?
                    .is_some_and(|container| container["State"]["Running"] == true),
                "cannot replace gateway: retained routes for application {id} lack a verified network and the old gateway is not running"
            );
            tracing::warn!(environment_id=%id,
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

    /// Performs a replacement inside the caller's action.
    ///
    /// 1. Validates the new Caddy configuration in a throwaway container.
    /// 2. Creates `-next` and attaches networks while the old gateway still serves.
    /// 3. Saves the live (or autosaved) configuration as `rollback.json`.
    /// 4. Stops the old gateway, renames it to `-previous`, renames `-next` to the
    ///    serving name, and starts it. On failure, restores the old gateway.
    /// 5. Deletes `rollback.json`, which commits the replacement.
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

    /// Runs `caddy validate` on the candidate configuration in a temporary
    /// container (no published ports, no restart), failing with its last log
    /// lines if validation exits non-zero. The container is removed afterwards.
    async fn validate_replacement(
        &self,
        journal: &Journaled<'_>,
        name: &str,
        spec: &Value,
        table: &RoutingTable,
    ) -> Result<()> {
        self.write_configuration("candidate.json", &self.configuration(table).await?)
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

    /// Renames a container.
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
    /// A `-previous` container without `rollback.json` means the cutover committed
    /// and only cleanup remains; any other leftover triggers a restore.
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

    /// Configuration saved before cutover; its presence means the replacement
    /// has not committed yet.
    fn rollback_path(&self) -> std::path::PathBuf {
        self.directory.join("config/caddy/rollback.json")
    }

    /// Reinstates the rollback configuration as Caddy's autosave.
    async fn restore_configuration(&self) -> Result<()> {
        let bytes = tokio::fs::read(self.rollback_path()).await?;
        self.write_configuration("autosave.json", &serde_json::from_slice(&bytes)?)
            .await
    }

    /// Atomically writes a JSON file under `config/caddy`, readable only by the
    /// daemon since it can hold DNS-01 private keys.
    async fn write_configuration(&self, file: &str, configuration: &Value) -> Result<()> {
        super::certificates::write_private(
            &self.directory.join("config/caddy").join(file),
            &serde_json::to_vec(configuration)?,
        )
        .await
    }

    /// Writes the configuration for `table` as the autosave and starts the gateway.
    async fn start_gateway(&self, journal: &Journaled<'_>, table: &RoutingTable) -> Result<()> {
        // A restarted gateway must never briefly resume routes that were removed
        // by deployments while ingress was disabled.
        self.write_configuration("autosave.json", &self.configuration(table).await?)
            .await?;
        self.start_container(journal).await
    }

    /// Starts the serving container and waits up to 10s for Caddy's admin API.
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

    /// Verifies an existing container with a matching spec hash still has the
    /// managed image, user, command, host security settings, port bindings, and
    /// environment, rather than trusting the hash label alone. Settings the spec
    /// leaves unset must be empty, as Docker reports them `null`, `[]` or `{}`.
    pub(super) fn check_container_configuration(current: &Value, spec: &Value) -> Result<()> {
        fn same(current: &Value, spec: &Value) -> bool {
            let empty = |value: &Value| {
                value.is_null()
                    || value.as_array().is_some_and(Vec::is_empty)
                    || value.as_object().is_some_and(serde_json::Map::is_empty)
            };
            current == spec || (empty(current) && empty(spec))
        }
        for field in ["Image", "User", "Cmd"] {
            ensure!(
                current["Config"][field] == spec[field],
                "managed container {field} conflicts with managed configuration"
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
                    "managed container restart policy conflicts"
                );
            } else {
                ensure!(
                    same(&current["HostConfig"][field], &spec["HostConfig"][field]),
                    "managed container host configuration {field} conflicts"
                );
            }
        }
        ensure!(
            same(
                &current["HostConfig"]["PortBindings"],
                &spec["HostConfig"]["PortBindings"]
            ),
            "managed container port bindings conflict"
        );
        let environment = current["Config"]["Env"]
            .as_array()
            .context("managed container environment is absent")?;
        for expected in spec["Env"]
            .as_array()
            .context("managed environment is absent")?
        {
            ensure!(
                environment.contains(expected),
                "managed container environment conflicts"
            );
        }
        ensure!(
            current["HostConfig"]["Privileged"] != true,
            "managed containers must not be privileged"
        );
        Ok(())
    }

    /// Connects the named container to each desired network it is not yet on.
    /// Application networks get `GwPriority` 0 so the edge network stays default.
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

    /// Desired networks the inspected container is not attached to.
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
    /// Attaches new networks before loading routes, and detaches stale networks
    /// only after the new configuration no longer references them.
    pub(super) async fn configure_gateway(
        &self,
        table: &RoutingTable,
        networks: &BTreeSet<String>,
    ) -> Result<()> {
        let container = self
            .container()
            .await?
            .context("gateway container disappeared")?;
        let desired = self.configuration(table).await?;
        let reload = self.caddy.get("/config/").await? != desired;
        // Keep existing attachments for unavailable apps whose routes were retained.
        let retained: BTreeSet<_> = table
            .iter()
            .filter(|(_, routes)| Self::proxies(routes))
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

    /// Forwards Caddy log lines emitted since the previous relay into daemon
    /// logs, and the apps node's at debug level since `tailscaled` is verbose
    /// (at most 100 lines per container and pass).
    pub(super) async fn relay_logs(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let mut since = self.logs_since.lock().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        for name in [self.name.clone(), self.node_name()] {
            if self.named_container(&name).await?.is_none() {
                continue;
            }
            let from = since.get(&name).copied().unwrap_or(0);
            let lines = self
                .container_logs(&name, &format!("tail=100&since={from}&until={now}"))
                .await?;
            for line in lines {
                if name == self.name {
                    tracing::info!(gateway=%name,caddy=%line,"Caddy diagnostic");
                } else {
                    tracing::debug!(node=%name,tailscale=%line,"apps tailnet node diagnostic");
                }
            }
            since.insert(name, now);
        }
        Ok(())
    }

    /// Reads container logs and decodes Docker's multiplexed stream framing into
    /// lines, each truncated to 4096 characters. `query` is appended to the URL.
    ///
    /// ```text
    /// [stream: u8][0; 3][length: u32 big-endian][payload: length bytes] ...
    /// ```
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
