use super::{
    ApplicationId, BTreeMap, BTreeSet, BollardDocker, DesiredNetwork, DesiredService,
    DesiredVolume, DockerApi, DockerError, DockerTimeout, HashMap, InspectNetworkOptions,
    InspectServiceOptions, Ipam, ListNetworksOptionsBuilder, ListServicesOptionsBuilder,
    ListTasksOptionsBuilder, ListVolumesOptionsBuilder, NetworkCreateRequest,
    OBSERVATION_INSPECT_CONCURRENCY, ObservedApplication, ObservedNetwork, ObservedService,
    ObservedVolume, ResourceKind, StreamExt, SwarmInitRequest, SwarmState, TryStreamExt,
    VolumeCreateOptions, async_trait, resolve_image_digest, stream,
};

/// Inspections attempted before an incomplete network response is an error.
const NETWORK_INSPECT_ATTEMPTS: usize = 10;
/// Pause between incomplete network inspections.
const NETWORK_INSPECT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

impl BollardDocker {
    /// Inspects networks and retains their ID-to-name mapping for services.
    ///
    /// Networks are listed by ownership label and by readable name prefix,
    /// deduplicated, then fully inspected concurrently. Networks that vanish
    /// mid-observation are skipped. The returned map covers every inspected
    /// network, while the observations keep only those `relevant` to the app.
    async fn snapshot_networks(
        &self,
        application: &ApplicationId,
    ) -> Result<(Vec<ObservedNetwork>, HashMap<String, String>), DockerError> {
        let mut raw_networks = Self::map_request(
            "list networks",
            self.docker
                .list_networks(Some(
                    ListNetworksOptionsBuilder::default()
                        .filters(&Self::application_label_filter(application))
                        .build(),
                ))
                .await,
        )?;
        let named_networks = Self::map_request(
            "list networks by name",
            self.docker
                .list_networks(Some(
                    ListNetworksOptionsBuilder::default()
                        .filters(&Self::application_name_filter(application))
                        .build(),
                ))
                .await,
        )?;
        let mut seen_networks = raw_networks
            .iter()
            .map(|network| (network.id.clone(), network.name.clone()))
            .collect::<BTreeSet<_>>();
        raw_networks.extend(
            named_networks
                .into_iter()
                .filter(|network| seen_networks.insert((network.id.clone(), network.name.clone()))),
        );
        let mut inspections = stream::iter(
            raw_networks
                .into_iter()
                .filter_map(|network| network.id.or(network.name)),
        )
        .map(|id| async {
            let inspected = self.inspect_network_complete(&id).await;
            (id, inspected)
        })
        .buffer_unordered(OBSERVATION_INSPECT_CONCURRENCY);
        let mut raw_networks = Vec::new();
        while let Some((id, inspected)) = inspections.next().await {
            match inspected {
                Ok(Some(network)) => raw_networks.push(network),
                Ok(None) => {
                    tracing::debug!(network_id = %id, "network vanished during observation");
                }
                Err(error) => return Err(error),
            }
        }
        let network_names = raw_networks
            .iter()
            .filter_map(|network| Some((network.id.clone()?, network.name.clone()?)))
            .collect::<HashMap<_, _>>();
        let networks = raw_networks
            .into_iter()
            .filter_map(|network| {
                let runtime_configuration_matches = Self::network_configuration_matches(&network);
                let name = network.name?;
                Some(ObservedNetwork {
                    name,
                    runtime_configuration_matches,
                    labels: network.labels.unwrap_or_default().into_iter().collect(),
                })
            })
            .filter(|r| Self::relevant(&r.name, &r.labels, application))
            .collect();
        Ok((networks, network_names))
    }

    /// Lists volumes by ownership label and by readable name prefix,
    /// deduplicated by name, and keeps those `relevant` to the application.
    ///
    /// Volumes are observed from the list responses without further inspection.
    async fn snapshot_volumes(
        &self,
        application: &ApplicationId,
    ) -> Result<Vec<ObservedVolume>, DockerError> {
        let mut raw_volumes = Self::map_request(
            "list volumes",
            self.docker
                .list_volumes(Some(
                    ListVolumesOptionsBuilder::default()
                        .filters(&Self::application_label_filter(application))
                        .build(),
                ))
                .await,
        )?
        .volumes
        .unwrap_or_default();
        let named_volumes = Self::map_request(
            "list volumes by name",
            self.docker
                .list_volumes(Some(
                    ListVolumesOptionsBuilder::default()
                        .filters(&Self::application_name_filter(application))
                        .build(),
                ))
                .await,
        )?
        .volumes
        .unwrap_or_default();
        let mut seen_volumes = raw_volumes
            .iter()
            .map(|volume| volume.name.clone())
            .collect::<BTreeSet<_>>();
        raw_volumes.extend(
            named_volumes
                .into_iter()
                .filter(|volume| seen_volumes.insert(volume.name.clone())),
        );
        let volumes = raw_volumes
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
            .collect();
        Ok(volumes)
    }

    /// Lists services by ownership label and by readable name prefix, then
    /// fully inspects each one concurrently.
    ///
    /// Returns the inspected services (skipping any that vanished) and the
    /// names of every listed service, used to filter the task listing.
    async fn inspect_application_services(
        &self,
        application: &ApplicationId,
    ) -> Result<(Vec<bollard::models::Service>, Vec<String>), DockerError> {
        let mut listed_services = Self::map_request(
            "list services",
            self.docker
                .list_services(Some(
                    ListServicesOptionsBuilder::default()
                        .filters(&Self::application_label_filter(application))
                        .status(true)
                        .build(),
                ))
                .await,
        )?;
        let named_services = Self::map_request(
            "list services by name",
            self.docker
                .list_services(Some(
                    ListServicesOptionsBuilder::default()
                        .filters(&Self::application_name_filter(application))
                        .status(true)
                        .build(),
                ))
                .await,
        )?;
        let mut seen_services = listed_services
            .iter()
            .map(|service| service.id.clone())
            .collect::<BTreeSet<_>>();
        listed_services.extend(
            named_services
                .into_iter()
                .filter(|service| seen_services.insert(service.id.clone())),
        );
        let service_names = listed_services
            .iter()
            .filter_map(|service| service.spec.as_ref()?.name.clone())
            .collect::<Vec<_>>();
        // Complete inspections run concurrently so one slow service cannot
        // serialize the whole snapshot.
        let mut inspections =
            stream::iter(listed_services.into_iter().filter_map(|listed| listed.id))
                .map(|id| async {
                    let inspected = self.inspect_service_wire(&id).await;
                    (id, inspected)
                })
                .buffer_unordered(OBSERVATION_INSPECT_CONCURRENCY);
        let mut raw_services = Vec::new();
        while let Some((id, inspected)) = inspections.next().await {
            match inspected {
                Ok(Some(service)) => raw_services.push(service),
                Ok(None) => {
                    tracing::debug!(service_id = %id, "service vanished during observation");
                }
                Err(error) => return Err(error),
            }
        }
        Ok((raw_services, service_names))
    }

    /// Observes the application's services together with their tasks.
    ///
    /// 1. Inspect candidate services and list all of their tasks at once.
    /// 2. Read container health for running, health-checked tasks.
    /// 3. Keep `relevant` services and convert each with its own tasks, checking
    ///    placement against the local `node_id`.
    /// 4. Replace network IDs in each service with names from `network_names`.
    async fn snapshot_services(
        &self,
        application: &ApplicationId,
        network_names: &HashMap<String, String>,
        node_id: &str,
    ) -> Result<Vec<piqueld_core::ObservedService>, DockerError> {
        let (raw_services, service_names) = self.inspect_application_services(application).await?;
        let all_tasks = if service_names.is_empty() {
            // Empty name filters rely on undocumented daemon behavior and
            // there is nothing to list for.
            Vec::new()
        } else {
            Self::map_request(
                "list tasks",
                self.docker
                    .list_tasks(Some(
                        ListTasksOptionsBuilder::default()
                            .filters(&HashMap::from([("service", service_names)]))
                            .build(),
                    ))
                    .await,
            )?
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
        for service in &mut services {
            Self::name_networks(service, network_names);
        }
        Ok(services)
    }

    /// Docker reports attachment targets as network IDs; planning compares names.
    fn name_networks(service: &mut ObservedService, network_names: &HashMap<String, String>) {
        for attachment in &mut service.networks {
            if let Some(name) = network_names.get(&attachment.network) {
                attachment.network = name.clone();
            }
        }
    }

    /// List responses can omit immutable network fields, so reconciliation
    /// decisions must use a complete inspection of the selected resource.
    ///
    /// Retries while the inspection lacks a driver or `attachable` flag, up to
    /// `NETWORK_INSPECT_ATTEMPTS`. Returns `None` when the network is gone.
    async fn inspect_network_complete(
        &self,
        identifier: &str,
    ) -> Result<Option<bollard::models::Network>, DockerError> {
        let mut attempt = 1;
        loop {
            match self
                .docker
                .inspect_network(identifier, None::<InspectNetworkOptions>)
                .await
            {
                Ok(network)
                    if network
                        .driver
                        .as_deref()
                        .is_some_and(|driver| !driver.is_empty())
                        && network.attachable.is_some() =>
                {
                    return Ok(Some(network));
                }
                Ok(_) if attempt == NETWORK_INSPECT_ATTEMPTS => {
                    return Err(DockerError::Request("read complete network inspection"));
                }
                Ok(_) => {
                    attempt += 1;
                    tokio::time::sleep(NETWORK_INSPECT_RETRY_DELAY).await;
                }
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => return Ok(None),
                Err(error) => return Err(DockerError::request("inspect network", error)),
            }
        }
    }
}

#[async_trait]
impl DockerApi for BollardDocker {
    async fn application_logs(
        &self,
        instance: &super::InstanceId,
        application: &ApplicationId,
        service: Option<&str>,
        tail: u16,
        since: u32,
        stream: Option<piqueld_core::api::LogStream>,
    ) -> Result<piqueld_core::api::ApplicationLogs, DockerError> {
        self.read_logs(instance, application, service, tail, since, stream)
            .await
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
                Self::map_request(
                    "initialize Docker Swarm",
                    self.docker
                        .init_swarm(SwarmInitRequest {
                            // Plan 06 is intentionally single-host. Do not expose the
                            // manager control port while bootstrapping the local Swarm.
                            listen_addr: Some("127.0.0.1:2377".into()),
                            advertise_addr: Some("127.0.0.1".into()),
                            ..Default::default()
                        })
                        .await,
                )?;
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
        dockerfile: &std::path::Path,
        context: &std::path::Path,
    ) -> Result<piqueld_core::resource::Sha256Digest, DockerError> {
        self.build_image_recorded(dockerfile, context, None).await
    }
    async fn build_image_recorded(
        &self,
        dockerfile: &std::path::Path,
        context: &std::path::Path,
        log: Option<&crate::build::BuildLog>,
    ) -> Result<piqueld_core::resource::Sha256Digest, DockerError> {
        // Builds shell out to the Docker CLI against the same socket and read
        // the image ID Docker writes to `--iidfile`, e.g. `sha256:<64 hex>`.
        use anyhow::Context;
        let result = async {
            if !dockerfile.is_file() || !context.is_dir() {
                anyhow::bail!("Dockerfile must be a file and build context must be a directory");
            }
            let directory = tempfile::tempdir().context("create Docker build directory")?;
            let iidfile = directory.path().join("image-id");
            let mut command = tokio::process::Command::new("docker");
            command
                .arg("--host")
                .arg(format!("unix://{}", self.socket.display()))
                .args(["build", "--pull", "--file"])
                .arg(dockerfile)
                .arg("--iidfile")
                .arg(&iidfile)
                .arg(context);
            crate::command::LoggedCommand::run_recorded(&mut command, "build Docker image", log)
                .await?;
            let id = tokio::fs::read_to_string(iidfile)
                .await
                .context("read built image ID")?;
            piqueld_core::resource::Sha256Digest::parse(id.trim())
                .context("validate built image ID")
        }
        .await;
        result.map_err(|source| DockerError::RequestSource {
            operation: "build Docker image",
            source: source.into(),
        })
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
        application: &ApplicationId,
    ) -> Result<ObservedApplication, DockerError> {
        // One deadline covers every phase, including complete resource inspections.
        DockerTimeout::Request
            .run("observe application", async {
                let node_id = self.local_node_id().await?;
                let (networks, network_names) = self.snapshot_networks(application).await?;
                let volumes = self.snapshot_volumes(application).await?;
                let services = self
                    .snapshot_services(application, &network_names, &node_id)
                    .await?;
                Ok(ObservedApplication {
                    networks,
                    volumes,
                    services,
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
                let existing = Self::map_request(
                    "find network by name",
                    self.docker
                        .list_networks(Some(
                            ListNetworksOptionsBuilder::default()
                                .filters(&HashMap::from([("name", vec![desired.name.to_string()])]))
                                .build(),
                        ))
                        .await,
                )?;
                if let Some(network) = existing
                    .into_iter()
                    .find(|n| n.name.as_deref() == Some(desired.name.as_str()))
                {
                    let Some(network) = self
                        .inspect_network_complete(
                            network.id.as_deref().unwrap_or(desired.name.as_str()),
                        )
                        .await?
                    else {
                        return Err(DockerError::Request("inspect existing network"));
                    };
                    let runtime_configuration_matches =
                        Self::network_configuration_matches(&network);
                    if !Self::owns_resource(
                        network.labels.unwrap_or_default(),
                        &desired.labels,
                        ResourceKind::Network,
                        desired.name.as_str(),
                    ) {
                        return Err(DockerError::OwnershipConflict);
                    }
                    if !runtime_configuration_matches {
                        return Err(DockerError::ConfigurationConflict);
                    }
                    return Ok(());
                }
                Self::map_request(
                    "create network",
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
                        .await,
                )
                .map(|_| ())
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
                let existing = Self::map_request(
                    "find volume by name",
                    self.docker
                        .list_volumes(Some(
                            ListVolumesOptionsBuilder::default()
                                .filters(&HashMap::from([("name", vec![desired.name.to_string()])]))
                                .build(),
                        ))
                        .await,
                )?
                .volumes
                .unwrap_or_default()
                .into_iter()
                .find(|v| v.name == desired.name.as_str());
                if let Some(volume) = existing {
                    let runtime_configuration_matches = Self::volume_configuration_matches(&volume);
                    if !Self::owns_resource(
                        volume.labels,
                        &desired.labels,
                        ResourceKind::Volume,
                        desired.name.as_str(),
                    ) {
                        return Err(DockerError::OwnershipConflict);
                    }
                    return if runtime_configuration_matches {
                        Ok(())
                    } else {
                        Err(DockerError::ConfigurationConflict)
                    };
                }
                Self::map_request(
                    "create volume",
                    self.docker
                        .create_volume(VolumeCreateOptions {
                            name: Some(desired.name.to_string()),
                            driver: Some("local".into()),
                            driver_opts: Some(HashMap::new()),
                            labels: Some(desired.labels.clone().into_iter().collect()),
                            ..Default::default()
                        })
                        .await,
                )
                .map(|_| ())
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
                    let matches = Self::map_request(
                        "find service by name",
                        self.docker
                            .list_services(Some(
                                ListServicesOptionsBuilder::default()
                                    .filters(&HashMap::from([(
                                        "name",
                                        vec![desired.name.to_string()],
                                    )]))
                                    .status(true)
                                    .build(),
                            ))
                            .await,
                    )?;
                    let node_id = self.local_node_id().await?;
                    let spec = self.service_spec_with_secrets(desired, &node_id).await?;
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
                            if !Self::owns_resource(
                                existing
                                    .spec
                                    .as_ref()
                                    .and_then(|s| s.labels.clone())
                                    .unwrap_or_default(),
                                &desired.labels,
                                ResourceKind::Service,
                                desired.name.as_str(),
                            ) {
                                return Err(DockerError::OwnershipConflict);
                            }
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
                            let networks = Self::map_request(
                                "list service networks",
                                self.docker
                                    .list_networks(Some(
                                        ListNetworksOptionsBuilder::default().build(),
                                    ))
                                    .await,
                            )?;
                            let network_names = networks
                                .into_iter()
                                .filter_map(|network| Some((network.id?, network.name?)))
                                .collect::<HashMap<_, _>>();
                            Self::name_networks(&mut observed, &network_names);
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
            .run("remove service", async {
                let existing = match self
                    .docker
                    .inspect_service(name, None::<InspectServiceOptions>)
                    .await
                {
                    Ok(value) => value,
                    Err(bollard::errors::Error::DockerResponseServerError {
                        status_code: 404,
                        ..
                    }) => return Ok(()),
                    Err(error) => return Err(DockerError::request("inspect service", error)),
                };
                let id = existing
                    .id
                    .clone()
                    .ok_or(DockerError::Request("read existing service identity"))?;
                let labels = existing.spec.and_then(|s| s.labels).unwrap_or_default();
                if !Self::owns_resource(labels, ownership, ResourceKind::Service, name) {
                    return Err(DockerError::OwnershipConflict);
                }
                // Delete the resource that was inspected, even if the name is replaced
                // between the ownership check and this request.
                match self.docker.delete_service(&id).await {
                    Ok(())
                    | Err(bollard::errors::Error::DockerResponseServerError {
                        status_code: 404,
                        ..
                    }) => Ok(()),
                    Err(error) => Err(DockerError::request("delete service", error)),
                }
            })
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
                let existing = match self
                    .docker
                    .inspect_network(name, None::<InspectNetworkOptions>)
                    .await
                {
                    Ok(value) => value,
                    Err(bollard::errors::Error::DockerResponseServerError {
                        status_code: 404,
                        ..
                    }) => return Ok(()),
                    Err(error) => return Err(DockerError::request("inspect network", error)),
                };
                let id = existing
                    .id
                    .clone()
                    .ok_or(DockerError::Request("read existing network identity"))?;
                let labels = existing.labels.unwrap_or_default();
                if !Self::owns_resource(labels, ownership, ResourceKind::Network, name) {
                    return Err(DockerError::OwnershipConflict);
                }
                // Network IDs make the ownership check and removal target the same object.
                match self.docker.remove_network(&id).await {
                    Ok(())
                    | Err(bollard::errors::Error::DockerResponseServerError {
                        status_code: 404,
                        ..
                    }) => Ok(()),
                    Err(error) => Err(DockerError::request("delete network", error)),
                }
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
