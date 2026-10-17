use super::{
    BTreeMap, BTreeSet, BollardDocker, BollardResult, DesiredNetwork, DesiredService,
    DesiredVolume, DockerApi, DockerError, DockerNetworkName, DockerTimeout, EnvironmentId,
    HashMap, InspectNetworkOptions, InspectServiceOptions, Ipam, ListNetworksOptionsBuilder,
    ListServicesOptionsBuilder, ListTasksOptionsBuilder, ListVolumesOptionsBuilder,
    NetworkCreateRequest, OBSERVATION_INSPECT_CONCURRENCY, ObservedApplication, ObservedNetwork,
    ObservedService, ObservedVolume, ResourceKind, StreamExt, SwarmInitRequest, SwarmState,
    TryStreamExt, VolumeCreateOptions, async_trait, resolve_image_digest, stream,
};

/// Inspections attempted before an incomplete network response is an error.
const NETWORK_INSPECT_ATTEMPTS: usize = 10;
/// Pause between incomplete network inspections.
const NETWORK_INSPECT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

/// An application's observed networks, with what service observation needs.
struct NetworkSnapshot {
    observed: Vec<ObservedNetwork>,
    /// Link networks other environments own for links to this one.
    linked: Vec<ObservedNetwork>,
    /// Network names keyed by the Docker IDs that service specs reference.
    names: HashMap<String, String>,
    /// Address range of the application's ingress network, when it exists.
    ingress_proxies: Option<String>,
}

/// Docker list filters, such as `{"label": ["<key>=<value>"]}`.
type Filters = HashMap<String, Vec<String>>;

impl BollardDocker {
    /// Lists resources with `list` under each of `listings`' filters and
    /// merges the listings, keeping the first resource of each `key`. Each
    /// listing's operation labels its failure.
    async fn list_unique<T, K: Ord, F>(
        listings: impl IntoIterator<Item = (&'static str, Filters)>,
        list: impl Fn(Filters) -> F,
        key: impl Fn(&T) -> K,
    ) -> Result<Vec<T>, DockerError>
    where
        F: Future<Output = Result<Vec<T>, bollard::errors::Error>>,
    {
        let mut unique = Vec::new();
        let mut seen = BTreeSet::new();
        for (operation, filters) in listings {
            let listed = list(filters).await.or_request(operation)?;
            unique.extend(listed.into_iter().filter(|item| seen.insert(key(item))));
        }
        Ok(unique)
    }

    /// The listings selecting an application's candidate resources: by
    /// ownership label, and by readable name prefix (see [`Self::relevant`]),
    /// labelled `by_label` and `by_name`.
    fn candidate_listings(
        application: &EnvironmentId,
        [by_label, by_name]: [&'static str; 2],
    ) -> [(&'static str, Filters); 2] {
        [
            (by_label, Self::application_label_filter(application)),
            (by_name, Self::application_name_filter(application)),
        ]
    }

    /// Runs `inspect` on every ID concurrently and keeps the inspections of
    /// resources that still exist; one that vanished mid-observation is
    /// skipped and reported to `vanished`.
    async fn inspect_all<T, F>(
        ids: impl IntoIterator<Item = String>,
        inspect: impl Fn(String) -> F,
        vanished: impl Fn(&str),
    ) -> Result<Vec<T>, DockerError>
    where
        F: Future<Output = (String, Result<Option<T>, DockerError>)>,
    {
        let mut inspections = stream::iter(ids)
            .map(inspect)
            .buffer_unordered(OBSERVATION_INSPECT_CONCURRENCY);
        let mut inspected = Vec::new();
        while let Some((id, result)) = inspections.next().await {
            if let Some(resource) = result? {
                inspected.push(resource);
            } else {
                vanished(&id);
            }
        }
        Ok(inspected)
    }

    /// Inspects networks and retains their ID-to-name mapping and the ingress
    /// network's address range for services.
    ///
    /// Candidate networks (see [`Self::candidate_listings`]) and the link
    /// networks importers own for links to the app are fully inspected
    /// concurrently. The name map covers every inspected network, while the
    /// observations keep only those `relevant` to the app, and the link
    /// networks of its importers.
    async fn snapshot_networks(
        &self,
        application: &EnvironmentId,
    ) -> Result<NetworkSnapshot, DockerError> {
        let [by_label, by_name] =
            Self::candidate_listings(application, ["list networks", "list networks by name"]);
        let listed = Self::list_unique(
            [
                by_label,
                by_name,
                ("list linked networks", Self::link_label_filter(application)),
            ],
            |filters| {
                self.docker.list_networks(Some(
                    ListNetworksOptionsBuilder::default()
                        .filters(&filters)
                        .build(),
                ))
            },
            |network| (network.id.clone(), network.name.clone()),
        )
        .await?;
        let networks = Self::inspect_all(
            listed
                .into_iter()
                .filter_map(|network| network.id.or(network.name)),
            |id| async move { (id.clone(), self.inspect_network_complete(&id).await) },
            |id| tracing::debug!(network_id = %id, "network vanished during observation"),
        )
        .await?;
        let names = networks
            .iter()
            .filter_map(|network| Some((network.id.clone()?, network.name.clone()?)))
            .collect::<HashMap<_, _>>();
        let ingress = DockerNetworkName::for_ingress(application);
        let ingress_proxies = networks
            .iter()
            .find(|network| network.name.as_deref() == Some(ingress.as_str()))
            .and_then(Self::ingress_proxies);
        let (observed, linked) = networks
            .into_iter()
            .filter_map(|network| {
                let runtime_configuration_matches = Self::network_configuration_matches(&network);
                // Overlay networks list a load balancer endpoint on each node
                // where any task uses them; only containers hold them.
                let in_use = network.containers.as_ref().is_some_and(|containers| {
                    containers
                        .keys()
                        .any(|endpoint| !endpoint.starts_with("lb-"))
                });
                let name = network.name?;
                Some(ObservedNetwork {
                    name,
                    runtime_configuration_matches,
                    labels: network.labels.unwrap_or_default().into_iter().collect(),
                    in_use,
                })
            })
            .filter(|network| {
                Self::relevant(&network.name, &network.labels, application)
                    || Self::linked(&network.labels, application)
            })
            .partition(|network| Self::relevant(&network.name, &network.labels, application));
        Ok(NetworkSnapshot {
            observed,
            linked,
            names,
            ingress_proxies,
        })
    }

    /// Lists candidate volumes (see [`Self::candidate_listings`]) and keeps
    /// those `relevant` to the application.
    ///
    /// Volumes are observed from the list responses without further inspection.
    async fn snapshot_volumes(
        &self,
        application: &EnvironmentId,
    ) -> Result<Vec<ObservedVolume>, DockerError> {
        let volumes = Self::list_unique(
            Self::candidate_listings(application, ["list volumes", "list volumes by name"]),
            |filters| async move {
                self.docker
                    .list_volumes(Some(
                        ListVolumesOptionsBuilder::default()
                            .filters(&filters)
                            .build(),
                    ))
                    .await
                    .map(|listed| listed.volumes.unwrap_or_default())
            },
            |volume| volume.name.clone(),
        )
        .await?;
        Ok(volumes
            .into_iter()
            .map(|volume| {
                let runtime_configuration_matches = Self::volume_configuration_matches(&volume);
                ObservedVolume {
                    name: volume.name,
                    runtime_configuration_matches,
                    labels: volume.labels.into_iter().collect(),
                }
            })
            .filter(|r| Self::relevant(&r.name, &r.labels, application))
            .collect())
    }

    /// Lists candidate services (see [`Self::candidate_listings`]), then
    /// fully inspects each one concurrently.
    ///
    /// Returns the inspected services (skipping any that vanished) and the
    /// names of every listed service, used to filter the task listing.
    async fn inspect_application_services(
        &self,
        application: &EnvironmentId,
    ) -> Result<(Vec<bollard::models::Service>, Vec<String>), DockerError> {
        let mut listed = Self::list_unique(
            Self::candidate_listings(application, ["list services", "list services by name"]),
            |filters| {
                self.docker.list_services(Some(
                    ListServicesOptionsBuilder::default()
                        .filters(&filters)
                        .status(true)
                        .build(),
                ))
            },
            |service| service.id.clone(),
        )
        .await?;
        // One-shot jobs run outside the maintained target. Planning, repair, and
        // health never see them; job execution and deletion remove them.
        listed.retain(|service| {
            !service
                .spec
                .as_ref()
                .and_then(|spec| spec.labels.as_ref())
                .is_some_and(|labels| labels.contains_key(piqueld_core::resource::JOB_LABEL))
        });
        let names = listed
            .iter()
            .filter_map(|service| service.spec.as_ref()?.name.clone())
            .collect::<Vec<_>>();
        // Complete inspections run concurrently so one slow service cannot
        // serialize the whole snapshot.
        let services = Self::inspect_all(
            listed.into_iter().filter_map(|listed| listed.id),
            |id| async move { (id.clone(), self.inspect_service_wire(&id).await) },
            |id| tracing::debug!(service_id = %id, "service vanished during observation"),
        )
        .await?;
        Ok((services, names))
    }

    /// Observes the application's services together with their tasks.
    ///
    /// 1. Inspect candidate services and list all of their tasks at once.
    /// 2. Read container health for running, health-checked tasks.
    /// 3. Keep `relevant` services and convert each with its own tasks, checking
    ///    placement against the local `node_id`.
    /// 4. Replace network IDs in each service with names from `networks`, and
    ///    check routed services' injected ingress range against the network.
    async fn snapshot_services(
        &self,
        application: &EnvironmentId,
        networks: &NetworkSnapshot,
        node_id: &str,
    ) -> Result<Vec<piqueld_core::ObservedService>, DockerError> {
        let (raw_services, service_names) = self.inspect_application_services(application).await?;
        let all_tasks = if service_names.is_empty() {
            // Empty name filters rely on undocumented daemon behavior and
            // there is nothing to list for.
            Vec::new()
        } else {
            self.docker
                .list_tasks(Some(
                    ListTasksOptionsBuilder::default()
                        .filters(&HashMap::from([("service", service_names)]))
                        .build(),
                ))
                .await
                .or_request("list tasks")?
        };
        let health_by_container = self
            .observe_running_health(&raw_services, &all_tasks)
            .await?;
        let mut services = raw_services
            .into_iter()
            .filter_map(|service| {
                let id = service.id.clone()?;
                let spec = service.spec?;
                let name = spec.name.clone()?;
                let labels: BTreeMap<_, _> = spec
                    .labels
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                if !Self::relevant(&name, &labels, application) {
                    return None;
                }
                let tasks = all_tasks
                    .iter()
                    .filter(|task| task.service_id.as_deref() == Some(&id))
                    .map(|task| {
                        let healthy = task
                            .status
                            .as_ref()
                            .and_then(|status| status.container_status.as_ref())
                            .and_then(|container| container.container_id.as_deref())
                            .and_then(|container| {
                                health_by_container.get(container).copied().flatten()
                            });
                        Self::observe_task(task, healthy)
                    })
                    .collect::<Vec<_>>();
                Some(Self::observe_service(
                    &spec,
                    node_id,
                    tasks,
                    service.update_status.and_then(|u| u.state),
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let ingress = DockerNetworkName::for_ingress(application);
        for service in &mut services {
            Self::name_networks(service, &networks.names);
            let attached = service
                .networks
                .iter()
                .any(|attachment| attachment.network == ingress.as_str());
            Self::observe_ingress_proxies(
                service,
                networks.ingress_proxies.as_deref().filter(|_| attached),
            );
        }
        Ok(services)
    }

    /// Replaces each attachment's network ID with its name from `network_names`
    /// (ID to name), because Docker reports targets as IDs while planning
    /// compares names. Unknown IDs are left as-is and so register as drift.
    fn name_networks(service: &mut ObservedService, network_names: &HashMap<String, String>) {
        for attachment in &mut service.networks {
            if let Some(name) = network_names.get(&attachment.network) {
                attachment.network = name.clone();
            }
        }
    }

    /// Deletes a service or job service after rechecking its ownership and role.
    pub(super) async fn remove_owned_service(
        &self,
        name: &str,
        ownership: &BTreeMap<String, String>,
        kind: ResourceKind,
    ) -> Result<(), DockerError> {
        let Some(existing) = self
            .docker
            .inspect_service(name, None::<InspectServiceOptions>)
            .await
            .found()
            .or_request("inspect service")?
        else {
            return Ok(());
        };
        let id = existing
            .id
            .clone()
            .ok_or(DockerError::Request("read existing service identity"))?;
        let labels = existing.spec.and_then(|s| s.labels).unwrap_or_default();
        Self::check_ownership(labels, ownership, kind, name)?;
        // Delete the resource that was inspected, even if the name is replaced
        // between the ownership check and this request.
        self.docker
            .delete_service(&id)
            .await
            .found()
            .or_request("delete service")?;
        Ok(())
    }

    /// List responses can omit immutable network fields, so reconciliation
    /// decisions must use a complete inspection of the selected resource.
    async fn inspect_network_complete(
        &self,
        identifier: &str,
    ) -> Result<Option<bollard::models::Network>, DockerError> {
        self.inspect_network_until(identifier, |_| true).await
    }

    /// Inspects a network until it is complete and `ready`.
    ///
    /// Retries while the inspection lacks a driver or `attachable` flag, or is
    /// not `ready`, up to `NETWORK_INSPECT_ATTEMPTS`. Returns `None` when the
    /// network is gone.
    pub(super) async fn inspect_network_until(
        &self,
        identifier: &str,
        ready: impl Fn(&bollard::models::Network) -> bool,
    ) -> Result<Option<bollard::models::Network>, DockerError> {
        for attempt in 1..=NETWORK_INSPECT_ATTEMPTS {
            let Some(network) = self
                .docker
                .inspect_network(identifier, None::<InspectNetworkOptions>)
                .await
                .found()
                .or_request("inspect network")?
            else {
                return Ok(None);
            };
            let complete = network
                .driver
                .as_deref()
                .is_some_and(|driver| !driver.is_empty())
                && network.attachable.is_some();
            if complete && ready(&network) {
                return Ok(Some(network));
            }
            if attempt < NETWORK_INSPECT_ATTEMPTS {
                tokio::time::sleep(NETWORK_INSPECT_RETRY_DELAY).await;
            }
        }
        Err(DockerError::Request("read complete network inspection"))
    }
}

#[async_trait]
impl DockerApi for BollardDocker {
    async fn application_logs(
        &self,
        instance: &super::InstanceId,
        application: &EnvironmentId,
        service: Option<&str>,
        tail: u16,
        since: u32,
        stream: Option<piqueld_core::api::LogStream>,
    ) -> Result<piqueld_core::api::ApplicationLogs, DockerError> {
        self.read_logs(instance, application, service, tail, since, stream)
            .await
    }

    async fn create_exec(
        &self,
        instance: &super::InstanceId,
        environment: &EnvironmentId,
        request: &piqueld_core::exec::ExecRequest,
    ) -> Result<Option<super::Exec>, DockerError> {
        DockerTimeout::Request
            .run(
                "create exec",
                self.create_task_exec(instance, environment, request),
            )
            .await
    }

    async fn run_exec(&self, exec: &super::Exec, io: super::ExecIo) -> Result<i64, DockerError> {
        self.run_task_exec(exec, io).await
    }

    async fn ping(&self) -> Result<(), DockerError> {
        DockerTimeout::Request
            .run("ping Docker", async {
                self.docker
                    .ping()
                    .await
                    .map(|_| ())
                    .map_err(|error| DockerError::unavailable("ping Docker", error))
            })
            .await
    }

    async fn ensure_secret(
        &self,
        name: &str,
        value: &[u8],
        ownership: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        DockerTimeout::Request
            .run(
                "ensure secret",
                Box::pin(self.provision_secret(name, value, ownership)),
            )
            .await
    }
    async fn remove_secrets(
        &self,
        names: &[String],
        ownership: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        DockerTimeout::Request
            .run(
                "remove secrets",
                Box::pin(self.remove_owned_secrets(names, ownership)),
            )
            .await
    }
    /// Accepts an existing single-node manager as `Ready`. An inactive node is
    /// initialized (loopback-only) when `auto_initialize` is set, then
    /// re-verified; any other node state is `NotManager`.
    async fn ensure_swarm(&self, auto_initialize: bool) -> Result<SwarmState, DockerError> {
        DockerTimeout::Request
            .run("ensure Docker Swarm", async {
                let swarm = self.swarm_info("inspect Docker Swarm state").await?;
                if swarm.control_available == Some(true) {
                    self.validate_single_node_manager().await?;
                    return Ok(SwarmState::Ready);
                }
                if swarm.local_node_state != Some(bollard::models::LocalNodeState::INACTIVE)
                    || !auto_initialize
                {
                    return Err(DockerError::NotManager);
                }
                self.docker
                    .init_swarm(SwarmInitRequest {
                        // Plan 06 is intentionally single-host. Do not expose the
                        // manager control port while bootstrapping the local Swarm.
                        listen_addr: Some("127.0.0.1:2377".into()),
                        advertise_addr: Some("127.0.0.1".into()),
                        ..Default::default()
                    })
                    .await
                    .or_request("initialize Docker Swarm")?;
                let checked = self.swarm_info("verify initialized Docker Swarm").await?;
                if checked.control_available != Some(true) {
                    return Err(DockerError::NotManager);
                }
                self.validate_single_node_manager().await?;
                Ok(SwarmState::Initialized)
            })
            .await
    }

    async fn build_image(
        &self,
        build: &super::ImageBuild<'_>,
        log: Option<&crate::build::BuildLog>,
    ) -> Result<piqueld_core::resource::Sha256Digest, DockerError> {
        // Builds shell out to Docker Buildx against the same socket, with the
        // engine's own builder: the account may have selected one that does not
        // load into the engine. Buildx also ignores `DOCKER_BUILDKIT=0`.
        // They read the image ID from the build's `--metadata-file`, e.g.
        // `{"containerimage.digest": "sha256:<64 hex>"}`. Provenance
        // attestations differ on every build and, with the containerd image
        // store, change that ID, so identical builds would never share a
        // release. Without them, `--iidfile` would hold the config digest,
        // which that store does not know the image by.
        use anyhow::Context;
        #[derive(serde::Deserialize)]
        struct Metadata {
            #[serde(rename = "containerimage.digest")]
            image_id: String,
        }
        let result = async {
            if !build.dockerfile.is_file() || !build.context.is_dir() {
                anyhow::bail!("Dockerfile must be a file and build context must be a directory");
            }
            let directory = tempfile::tempdir().context("create Docker build directory")?;
            let metadata = directory.path().join("metadata.json");
            let mut command = tokio::process::Command::new("docker");
            command
                .arg("--host")
                .arg(format!("unix://{}", self.socket.display()))
                .args(["buildx", "build", "--builder=default", "--pull"])
                .args(["--provenance=false", "--file"])
                .arg(&build.dockerfile)
                .arg("--metadata-file")
                .arg(&metadata)
                .args(build.options())
                .arg(&build.context);
            crate::command::LoggedCommand::run_recorded(&mut command, "build Docker image", log)
                .await?;
            let metadata = tokio::fs::read(metadata)
                .await
                .context("read build metadata")?;
            let metadata: Metadata =
                serde_json::from_slice(&metadata).context("decode build metadata")?;
            piqueld_core::resource::Sha256Digest::parse(metadata.image_id)
                .context("validate built image ID")
        }
        .await;
        result.map_err(|source| DockerError::RequestSource {
            operation: "build Docker image",
            source: source.into(),
        })
    }

    async fn images(&self) -> Result<Vec<super::LocalImage>, DockerError> {
        DockerTimeout::Request
            .run("list images", self.list_local_images())
            .await
    }

    async fn remove_image(
        &self,
        instance: &piqueld_core::InstanceId,
        id: &piqueld_core::Sha256Digest,
    ) -> Result<(), DockerError> {
        DockerTimeout::Request
            .run("remove image", self.remove_built_image(instance, id))
            .await
    }

    async fn prune_health_probes(&self) -> Result<(), DockerError> {
        DockerTimeout::Request
            .run("prune health probes", self.remove_stale_health_probes())
            .await
    }

    async fn prune_files(&self, ownership: &BTreeMap<String, String>) -> Result<(), DockerError> {
        DockerTimeout::Request
            .run("prune service files", self.remove_unused_files(ownership))
            .await
    }

    async fn resolve_image(&self, reference: &str) -> Result<String, DockerError> {
        // Pulling through the Engine records RepoDigests, and resolution
        // verifies the tag was not re-pointed while the pull ran. Stream
        // details are intentionally discarded because image-pull progress is
        // not part of the durable API contract. A cold pull of a large image
        // exceeds the per-request budget, so resolution carries its own.
        DockerTimeout::ImageResolution
            .run(
                "resolve image",
                resolve_image_digest(self.docker.as_ref(), reference),
            )
            .await
    }

    async fn observe(
        &self,
        application: &EnvironmentId,
    ) -> Result<ObservedApplication, DockerError> {
        // One deadline covers every phase, including complete resource inspections.
        DockerTimeout::Request
            .run("observe application", async {
                let node_id = self.local_node_id().await?;
                let networks = self.snapshot_networks(application).await?;
                let volumes = self.snapshot_volumes(application).await?;
                let services = self
                    .snapshot_services(application, &networks, &node_id)
                    .await?;
                Ok(ObservedApplication {
                    networks: networks.observed,
                    volumes,
                    services,
                    linked_networks: networks.linked,
                })
            })
            .await
    }

    /// Creates the overlay network, or verifies an existing one with the same
    /// name: foreign ownership, a wrong resource role, or a non-canonical name is
    /// `OwnershipConflict`, and mismatched immutable settings are
    /// `ConfigurationConflict`.
    async fn ensure_network(&self, desired: &DesiredNetwork) -> Result<(), DockerError> {
        DockerTimeout::Request
            .run("ensure network", async {
                if !desired.has_valid_identity() {
                    return Err(DockerError::OwnershipConflict);
                }
                let existing = self
                    .docker
                    .list_networks(Some(
                        ListNetworksOptionsBuilder::default()
                            .filters(&HashMap::from([("name", vec![desired.name.to_string()])]))
                            .build(),
                    ))
                    .await
                    .or_request("find network by name")?;
                if let Some(network) = existing
                    .into_iter()
                    .find(|n| n.name.as_deref() == Some(desired.name.as_str()))
                {
                    let network = self
                        .inspect_network_complete(
                            network.id.as_deref().unwrap_or(desired.name.as_str()),
                        )
                        .await?
                        .ok_or(DockerError::Request("inspect existing network"))?;
                    let runtime_configuration_matches =
                        Self::network_configuration_matches(&network);
                    return Self::check_existing(
                        network.labels.unwrap_or_default(),
                        &desired.labels,
                        ResourceKind::Network,
                        desired.name.as_str(),
                        runtime_configuration_matches,
                    );
                }
                self.docker
                    .create_network(NetworkCreateRequest {
                        name: desired.name.to_string(),
                        driver: Some("overlay".into()),
                        internal: Some(false),
                        attachable: Some(true),
                        ingress: Some(false),
                        ipam: Some(Ipam::default()),
                        enable_ipv6: Some(false),
                        options: Some(HashMap::new()),
                        labels: Some(desired.labels.clone().into_iter().collect()),
                        ..Default::default()
                    })
                    .await
                    .or_request("create network")?;
                Ok(())
            })
            .await
    }

    /// Creates the local volume, or verifies an existing one with the same
    /// name using the same ownership and configuration rules as networks.
    async fn ensure_volume(&self, desired: &DesiredVolume) -> Result<(), DockerError> {
        DockerTimeout::Request
            .run("ensure volume", async {
                if !desired.has_valid_identity() {
                    return Err(DockerError::OwnershipConflict);
                }
                let existing = self
                    .docker
                    .list_volumes(Some(
                        ListVolumesOptionsBuilder::default()
                            .filters(&HashMap::from([("name", vec![desired.name.to_string()])]))
                            .build(),
                    ))
                    .await
                    .or_request("find volume by name")?
                    .volumes
                    .unwrap_or_default()
                    .into_iter()
                    .find(|v| v.name == desired.name.as_str());
                if let Some(volume) = existing {
                    let runtime_configuration_matches = Self::volume_configuration_matches(&volume);
                    return Self::check_existing(
                        volume.labels,
                        &desired.labels,
                        ResourceKind::Volume,
                        desired.name.as_str(),
                        runtime_configuration_matches,
                    );
                }
                self.docker
                    .create_volume(VolumeCreateOptions {
                        name: Some(desired.name.to_string()),
                        driver: Some("local".into()),
                        driver_opts: Some(HashMap::new()),
                        labels: Some(desired.labels.clone().into_iter().collect()),
                        ..Default::default()
                    })
                    .await
                    .or_request("create volume")?;
                Ok(())
            })
            .await
    }

    /// Creates the service, or updates an owned one whose observed state no
    /// longer matches `desired`.
    ///
    /// 1. Build the desired spec pinned to the local node, attaching owned secret
    ///    references by ID.
    /// 2. If a service with the name exists, inspect it completely, reject
    ///    foreign ownership, and translate its network IDs to names.
    /// 3. Return early when the observation already matches; otherwise update
    ///    the inspected service ID at its inspected version. A service that
    ///    vanished is recreated.
    async fn ensure_service(&self, desired: &DesiredService) -> Result<(), DockerError> {
        DockerTimeout::Request
            .run(
                "ensure service",
                Box::pin(async {
                    if !desired.has_valid_identity() {
                        return Err(DockerError::OwnershipConflict);
                    }
                    self.check_volumes_free(desired).await?;
                    let matches = self
                        .docker
                        .list_services(Some(
                            ListServicesOptionsBuilder::default()
                                .filters(&HashMap::from([("name", vec![desired.name.to_string()])]))
                                .status(true)
                                .build(),
                        ))
                        .await
                        .or_request("find service by name")?;
                    let node_id = self.local_node_id().await?;
                    let spec = self.runtime_service_spec(desired, &node_id).await?;
                    match matches.into_iter().find(|s| {
                        s.spec.as_ref().and_then(|s| s.name.as_deref())
                            == Some(desired.name.as_str())
                    }) {
                        Some(existing) => {
                            // List responses can omit fields needed for semantic comparison.
                            // Always use the complete service inspection before observing or
                            // deciding whether an update is necessary.
                            let inspected = self
                                .inspect_service_wire(
                                    existing.id.as_deref().unwrap_or(desired.name.as_str()),
                                )
                                .await?;
                            let Some(existing) = inspected else {
                                return self.create_service_wire(&spec).await;
                            };
                            Self::check_ownership(
                                existing
                                    .spec
                                    .as_ref()
                                    .and_then(|s| s.labels.clone())
                                    .unwrap_or_default(),
                                &desired.labels,
                                ResourceKind::Service,
                                desired.name.as_str(),
                            )?;
                            let existing_spec = existing.spec.as_ref().ok_or(
                                DockerError::Request("read existing service specification"),
                            )?;
                            let mut observed = Self::observe_service(
                                existing_spec,
                                &node_id,
                                Vec::new(),
                                existing
                                    .update_status
                                    .as_ref()
                                    .and_then(|status| status.state),
                            )?;
                            let networks = self
                                .docker
                                .list_networks(Some(ListNetworksOptionsBuilder::default().build()))
                                .await
                                .or_request("list service networks")?;
                            let network_names = networks
                                .into_iter()
                                .filter_map(|network| Some((network.id?, network.name?)))
                                .collect::<HashMap<_, _>>();
                            Self::name_networks(&mut observed, &network_names);
                            Self::observe_ingress_proxies(
                                &mut observed,
                                Self::spec_ingress_proxies(&spec),
                            );
                            if observed.matches(desired) {
                                return Ok(());
                            }
                            let version = existing
                                .version
                                .and_then(|v| v.index)
                                .ok_or(DockerError::Request("read existing service version"))?;
                            let id = existing
                                .id
                                .as_deref()
                                .ok_or(DockerError::Request("read existing service identity"))?;
                            self.update_service_wire(id, version, &spec).await
                        }
                        None => self.create_service_wire(&spec).await,
                    }
                }),
            )
            .await
    }

    /// Removes the service by its inspected ID after rechecking ownership and
    /// canonical name. A missing service counts as removed.
    async fn remove_service(
        &self,
        name: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        DockerTimeout::Request
            .run(
                "remove service",
                self.remove_owned_service(name, ownership, ResourceKind::Service),
            )
            .await
    }
    /// Removes the network from the inspected service's current
    /// specification and updates it at the inspected version, so a pending
    /// rollout is neither applied nor reverted. A concurrent update is not
    /// retried with the stale specification; the next scan detaches again.
    async fn detach_network(
        &self,
        service: &str,
        network: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        DockerTimeout::Request
            .run(
                "detach network",
                Box::pin(async {
                    let Some(existing) = self.inspect_service_wire(service).await? else {
                        return Ok(());
                    };
                    let mut spec = existing
                        .spec
                        .ok_or(DockerError::Request("read existing service specification"))?;
                    Self::check_ownership(
                        spec.labels.clone().unwrap_or_default(),
                        ownership,
                        ResourceKind::Service,
                        service,
                    )?;
                    let Some(inspected) = self
                        .docker
                        .inspect_network(network, None::<InspectNetworkOptions>)
                        .await
                        .found()
                        .or_request("inspect network")?
                    else {
                        return Ok(());
                    };
                    let network_id = inspected.id;
                    let Some(attachments) = spec
                        .task_template
                        .as_mut()
                        .and_then(|template| template.networks.as_mut())
                    else {
                        return Ok(());
                    };
                    let attached = attachments.len();
                    attachments.retain(|attachment| {
                        let target = attachment.target.as_deref();
                        target != Some(network) && target != network_id.as_deref()
                    });
                    if attachments.len() == attached {
                        return Ok(());
                    }
                    let version = existing
                        .version
                        .and_then(|version| version.index)
                        .ok_or(DockerError::Request("read existing service version"))?;
                    let id = existing
                        .id
                        .as_deref()
                        .ok_or(DockerError::Request("read existing service identity"))?;
                    self.post_service_update(id, version, &spec)
                        .await
                        .map_err(|error| error.sanitized("detach network"))
                }),
            )
            .await
    }
    async fn start_job(&self, job: &piqueld_core::DesiredJobRun) -> Result<(), DockerError> {
        DockerTimeout::Request
            .run("start job", Box::pin(self.create_job(job)))
            .await
    }
    async fn job_status(
        &self,
        job: &piqueld_core::DesiredJobRun,
    ) -> Result<super::JobStatus, DockerError> {
        DockerTimeout::Request
            .run("inspect job", self.inspect_job(job))
            .await
    }
    async fn job_output(
        &self,
        job: &piqueld_core::DesiredJobRun,
    ) -> Result<super::JobOutput, DockerError> {
        DockerTimeout::Request
            .run("read job output", self.read_job_output(job))
            .await
    }
    async fn remove_jobs(
        &self,
        ownership: &BTreeMap<String, String>,
        runs: super::JobRuns<'_>,
    ) -> Result<(), DockerError> {
        DockerTimeout::Request
            .run("remove jobs", self.remove_owned_jobs(ownership, runs))
            .await
    }
    /// Removes the private network by its inspected ID after rechecking
    /// ownership, role, and canonical name. A missing network counts as removed.
    async fn remove_network(
        &self,
        name: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        DockerTimeout::Request
            .run("remove network", async {
                let Some(existing) = self
                    .docker
                    .inspect_network(name, None::<InspectNetworkOptions>)
                    .await
                    .found()
                    .or_request("inspect network")?
                else {
                    return Ok(());
                };
                let id = existing
                    .id
                    .clone()
                    .ok_or(DockerError::Request("read existing network identity"))?;
                let labels = existing.labels.unwrap_or_default();
                Self::check_ownership(labels, ownership, ResourceKind::Network, name)?;
                // Network IDs make the ownership check and removal target the same object.
                self.docker
                    .remove_network(&id)
                    .await
                    .found()
                    .or_request("delete network")?;
                Ok(())
            })
            .await
    }

    /// Removes the volume by name after rechecking its ownership. Volume
    /// names are unique and cannot be reused while it exists, so the check
    /// and removal target the same volume. A volume still in use fails, and
    /// a missing one counts as removed.
    async fn remove_volume(
        &self,
        name: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        DockerTimeout::Request
            .run("remove volume", async {
                let Some(existing) = self
                    .docker
                    .inspect_volume(name)
                    .await
                    .found()
                    .or_request("inspect volume")?
                else {
                    return Ok(());
                };
                Self::check_ownership(existing.labels, ownership, ResourceKind::Volume, name)?;
                self.docker
                    .remove_volume(name, None::<bollard::query_parameters::RemoveVolumeOptions>)
                    .await
                    .found()
                    .or_request("delete volume")?;
                Ok(())
            })
            .await
    }
}

impl BollardDocker {
    /// Inspects the containers of running tasks that declare a healthcheck and
    /// maps each container ID to its current verdict.
    ///
    /// Services without a healthcheck are never inspected: their tasks stay
    /// without a verdict and observation cost stays proportional to the
    /// health-checked workload.
    async fn observe_running_health(
        &self,
        services: &[bollard::models::Service],
        tasks: &[bollard::models::Task],
    ) -> Result<HashMap<String, Option<bool>>, DockerError> {
        let healthchecked = services
            .iter()
            .filter(|service| {
                service
                    .spec
                    .as_ref()
                    .and_then(|spec| spec.task_template.as_ref())
                    .and_then(|task| task.container_spec.as_ref())
                    .is_some_and(|container| container.health_check.is_some())
            })
            .filter_map(|service| service.id.clone())
            .collect::<BTreeSet<_>>();
        let containers = tasks
            .iter()
            .filter(|task| {
                let running = task.status.as_ref().and_then(|status| status.state)
                    == Some(bollard::models::TaskState::RUNNING);
                let desired = task.desired_state == Some(bollard::models::TaskState::RUNNING);
                let healthchecked = task
                    .service_id
                    .as_deref()
                    .is_some_and(|id| healthchecked.contains(id));
                running && desired && healthchecked
            })
            .filter_map(|task| {
                task.status
                    .as_ref()
                    .and_then(|status| status.container_status.as_ref())
                    .and_then(|container| container.container_id.clone())
            })
            .collect::<Vec<_>>();
        stream::iter(containers)
            .map(|container_id| async {
                let verdict = self.container_health(&container_id).await?;
                Ok::<_, DockerError>((container_id, verdict))
            })
            .buffer_unordered(OBSERVATION_INSPECT_CONCURRENCY)
            .try_collect::<HashMap<_, _>>()
            .await
    }
}
