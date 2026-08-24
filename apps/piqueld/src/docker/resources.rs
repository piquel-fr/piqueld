use super::{
    ApplicationId, BTreeMap, BTreeSet, BollardDocker, DesiredNetwork, DesiredService,
    DesiredVolume, DockerApi, DockerError, HashMap, IMAGE_RESOLVE_TIMEOUT, InspectNetworkOptions,
    InspectServiceOptions, Ipam, ListNetworksOptionsBuilder, ListServicesOptionsBuilder,
    ListTasksOptionsBuilder, ListVolumesOptionsBuilder, NetworkCreateRequest,
    OBSERVATION_INSPECT_CONCURRENCY, ObservedApplication, ObservedNetwork, ObservedVolume,
    SERVICE_LABEL, StreamExt, SwarmInitRequest, SwarmState, TryStreamExt, VolumeCreateOptions,
    async_trait, bounded, resolve_image_digest, stream,
};

#[async_trait]
impl DockerApi for BollardDocker {
    async fn ensure_swarm(&self, auto_initialize: bool) -> Result<SwarmState, DockerError> {
        bounded("ensure Docker Swarm", async {
            let info =
                self.docker.info().await.map_err(|error| {
                    DockerError::unavailable("inspect Docker Swarm state", error)
                })?;
            let swarm = info.swarm.unwrap_or_default();
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
            let checked = self.docker.info().await.map_err(|error| {
                DockerError::unavailable("verify initialized Docker Swarm", error)
            })?;
            if checked
                .swarm
                .is_none_or(|s| s.control_available != Some(true))
            {
                return Err(DockerError::NotManager);
            }
            self.validate_single_node_manager().await?;
            Ok(SwarmState::Initialized)
        })
        .await
    }

    async fn resolve_image(&self, reference: &str) -> Result<String, DockerError> {
        // Pulling through the Engine records RepoDigests, and resolution
        // verifies the tag was not re-pointed while the pull ran. Stream
        // details are intentionally discarded because image-pull progress is
        // not part of the durable API contract. A cold pull of a large image
        // exceeds the per-request budget, so resolution carries its own.
        match tokio::time::timeout(
            IMAGE_RESOLVE_TIMEOUT,
            resolve_image_digest(self.docker.as_ref(), reference),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(DockerError::Unavailable("resolve image")),
        }
    }

    // Keep the correlated resource snapshot in one boundary operation so all
    // resource IDs can be normalized before service comparisons.
    #[allow(clippy::too_many_lines)]
    async fn observe(
        &self,
        application: &ApplicationId,
    ) -> Result<ObservedApplication, DockerError> {
        bounded("observe application", async {
            let raw_networks = Self::map_request(
                "list networks",
                self.docker
                    .list_networks(Some(
                        ListNetworksOptionsBuilder::default()
                            .filters(&Self::application_label_filter(application))
                            .build(),
                    ))
                    .await,
            )?;
            let network_names = raw_networks
                .iter()
                .filter_map(|network| Some((network.id.clone()?, network.name.clone()?)))
                .collect::<HashMap<_, _>>();
            let networks = raw_networks
                .into_iter()
                .filter_map(|network| {
                    let runtime_configuration_matches =
                        Self::network_configuration_matches(&network);
                    let name = network.name?;
                    Some(ObservedNetwork {
                        name,
                        runtime_configuration_matches,
                        labels: network.labels.unwrap_or_default().into_iter().collect(),
                    })
                })
                .filter(|r| Self::relevant(&r.name, &r.labels, application))
                .collect();
            let volumes = Self::map_request(
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
            .unwrap_or_default()
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
            let listed_services = Self::map_request(
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
                        tasks,
                        service.update_status.and_then(|u| u.state),
                    ))
                })
                .collect::<Result<Vec<_>, _>>()?;
            for service in &mut services {
                for target in &mut service.networks {
                    if let Some(name) = network_names.get(target) {
                        *target = name.clone();
                    }
                }
            }
            Ok(ObservedApplication {
                networks,
                volumes,
                services,
            })
        })
        .await
    }

    async fn ensure_network(&self, desired: &DesiredNetwork) -> Result<(), DockerError> {
        bounded("ensure network", async {
            if !desired.has_valid_identity() {
                return Err(DockerError::OwnershipConflict);
            }
            let existing = Self::map_request(
                "find network by name",
                self.docker
                    .list_networks(Some(
                        ListNetworksOptionsBuilder::default()
                            .filters(&HashMap::from([("name", vec![desired.name.clone()])]))
                            .build(),
                    ))
                    .await,
            )?;
            if let Some(network) = existing
                .into_iter()
                .find(|n| n.name.as_deref() == Some(&desired.name))
            {
                let runtime_configuration_matches = Self::network_configuration_matches(&network);
                let labels = network
                    .labels
                    .unwrap_or_default()
                    .into_iter()
                    .collect::<BTreeMap<_, _>>();
                let wrong_resource_role = labels.contains_key(SERVICE_LABEL);
                if !Self::owns(&labels, &desired.labels) || wrong_resource_role {
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
                        name: desired.name.clone(),
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

    async fn ensure_volume(&self, desired: &DesiredVolume) -> Result<(), DockerError> {
        bounded("ensure volume", async {
            if !desired.has_valid_identity() {
                return Err(DockerError::OwnershipConflict);
            }
            let existing = Self::map_request(
                "find volume by name",
                self.docker
                    .list_volumes(Some(
                        ListVolumesOptionsBuilder::default()
                            .filters(&HashMap::from([("name", vec![desired.name.clone()])]))
                            .build(),
                    ))
                    .await,
            )?
            .volumes
            .unwrap_or_default()
            .into_iter()
            .find(|v| v.name == desired.name);
            if let Some(volume) = existing {
                let runtime_configuration_matches = Self::volume_configuration_matches(&volume);
                let labels = volume.labels.into_iter().collect::<BTreeMap<_, _>>();
                if !Self::owns(&labels, &desired.labels) || labels.contains_key(SERVICE_LABEL) {
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
                        name: Some(desired.name.clone()),
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

    async fn ensure_service(&self, desired: &DesiredService) -> Result<(), DockerError> {
        bounded(
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
                                .filters(&HashMap::from([("name", vec![desired.name.clone()])]))
                                .status(true)
                                .build(),
                        ))
                        .await,
                )?;
                let spec = Self::service_spec(desired)?;
                match matches.into_iter().find(|s| {
                    s.spec.as_ref().and_then(|s| s.name.as_deref()) == Some(&desired.name)
                }) {
                    Some(existing) => {
                        // List responses can omit fields needed for semantic comparison.
                        // Always use the complete service inspection before observing or
                        // deciding whether an update is necessary.
                        let inspected = self
                            .inspect_service_wire(existing.id.as_deref().unwrap_or(&desired.name))
                            .await?;
                        let Some(existing) = inspected else {
                            return self.create_service_wire(&spec).await;
                        };
                        let labels: BTreeMap<_, _> = existing
                            .spec
                            .as_ref()
                            .and_then(|s| s.labels.clone())
                            .unwrap_or_default()
                            .into_iter()
                            .collect();
                        if !Self::owns(&labels, &desired.labels) {
                            return Err(DockerError::OwnershipConflict);
                        }
                        let existing_spec = existing
                            .spec
                            .as_ref()
                            .ok_or(DockerError::Request("read existing service specification"))?;
                        let mut observed = Self::observe_service(
                            existing_spec,
                            Vec::new(),
                            existing
                                .update_status
                                .as_ref()
                                .and_then(|status| status.state),
                        )?;
                        let networks = Self::map_request(
                            "list service networks",
                            self.docker
                                .list_networks(Some(ListNetworksOptionsBuilder::default().build()))
                                .await,
                        )?;
                        let network_names = networks
                            .into_iter()
                            .filter_map(|network| Some((network.id?, network.name?)))
                            .collect::<HashMap<_, _>>();
                        for target in &mut observed.networks {
                            if let Some(name) = network_names.get(target) {
                                *target = name.clone();
                            }
                        }
                        if observed.matches(desired) {
                            return Ok(());
                        }
                        let version = existing
                            .version
                            .and_then(|v| v.index)
                            .ok_or(DockerError::Request("read existing service version"))?;
                        self.update_service_wire(&desired.name, version, &spec)
                            .await
                    }
                    None => self.create_service_wire(&spec).await,
                }
            }),
        )
        .await
    }

    async fn remove_service(
        &self,
        name: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        bounded("remove service", async {
            let existing = match self
                .docker
                .inspect_service(name, None::<InspectServiceOptions>)
                .await
            {
                Ok(value) => value,
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => return Ok(()),
                Err(error) => return Err(DockerError::request("inspect service", error)),
            };
            let id = existing
                .id
                .clone()
                .ok_or(DockerError::Request("read existing service identity"))?;
            let labels: BTreeMap<_, _> = existing
                .spec
                .and_then(|s| s.labels)
                .unwrap_or_default()
                .into_iter()
                .collect();
            if !Self::owns_named_service(&labels, ownership, name) {
                return Err(DockerError::OwnershipConflict);
            }
            // Delete the resource that was inspected, even if the name is replaced
            // between the ownership check and this request.
            match self.docker.delete_service(&id).await {
                Ok(())
                | Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => Ok(()),
                Err(error) => Err(DockerError::request("delete service", error)),
            }
        })
        .await
    }
    async fn remove_network(
        &self,
        name: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        bounded("remove network", async {
            let existing = match self
                .docker
                .inspect_network(name, None::<InspectNetworkOptions>)
                .await
            {
                Ok(value) => value,
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => return Ok(()),
                Err(error) => return Err(DockerError::request("inspect network", error)),
            };
            let id = existing
                .id
                .clone()
                .ok_or(DockerError::Request("read existing network identity"))?;
            let labels: BTreeMap<_, _> = existing.labels.unwrap_or_default().into_iter().collect();
            if !Self::owns_private_network(&labels, ownership, name) {
                return Err(DockerError::OwnershipConflict);
            }
            // Network IDs make the ownership check and removal target the same object.
            match self.docker.remove_network(&id).await {
                Ok(())
                | Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
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
