use super::{
    APPLICATION_LABEL, ApplicationId, BTreeMap, BollardDocker, CreateImageOptionsBuilder,
    DesiredNetwork, DesiredSecret, DesiredService, DesiredVolume, DockerApi, DockerError, HashMap,
    InspectNetworkOptions, InspectServiceOptions, Ipam, ListNetworksOptionsBuilder,
    ListServicesOptionsBuilder, ListTasksOptionsBuilder, ListVolumesOptionsBuilder,
    NetworkCreateRequest, ObservedApplication, ObservedNetwork, ObservedSecret, ObservedVolume,
    SECRET_LABEL, SERVICE_LABEL, SwarmInitRequest, SwarmState, TryStreamExt, VolumeCreateOptions,
    async_trait,
};
use base64::Engine as _;
use piqueld_core::resource::image_repository;

impl BollardDocker {
    async fn observe_secret_resources(
        &self,
        services: &[bollard::models::Service],
        application: &ApplicationId,
    ) -> Result<Vec<ObservedSecret>, DockerError> {
        let used = services
            .iter()
            .filter_map(|service| service.spec.as_ref())
            .flat_map(|spec| {
                spec.task_template
                    .as_ref()
                    .and_then(|task| task.container_spec.as_ref())
                    .and_then(|container| container.secrets.as_ref())
                    .into_iter()
                    .flatten()
            })
            .filter_map(|reference| reference.secret_name.clone())
            .collect::<std::collections::BTreeSet<_>>();
        Ok(Self::map_request(
            "list secrets",
            self.docker
                .list_secrets(None::<bollard::query_parameters::ListSecretsOptions>)
                .await,
        )?
        .into_iter()
        .filter_map(|secret| {
            let spec = secret.spec?;
            let name = spec.name?;
            let labels = spec.labels.unwrap_or_default().into_iter().collect();
            Self::relevant(&name, &labels, application).then_some(ObservedSecret {
                in_use: used.contains(&name),
                name,
                labels,
            })
        })
        .collect())
    }

    async fn resolve_service_secret_ids(
        &self,
        desired: &DesiredService,
        spec: &mut super::ServiceSpec,
    ) -> Result<(), DockerError> {
        if desired.secrets.is_empty() {
            return Ok(());
        }
        let ids = Self::map_request(
            "list service secrets",
            self.docker
                .list_secrets(None::<bollard::query_parameters::ListSecretsOptions>)
                .await,
        )?
        .into_iter()
        .filter_map(|secret| Some((secret.spec?.name?, secret.id?)))
        .collect::<BTreeMap<_, _>>();
        let references = spec
            .task_template
            .as_mut()
            .and_then(|task| task.container_spec.as_mut())
            .and_then(|container| container.secrets.as_mut())
            .ok_or(DockerError::Request("build service secret references"))?;
        for reference in references {
            let name = reference
                .secret_name
                .as_ref()
                .ok_or(DockerError::Request("build service secret references"))?;
            reference.secret_id = Some(
                ids.get(name)
                    .cloned()
                    .ok_or(DockerError::Request("resolve service secret identity"))?,
            );
        }
        Ok(())
    }
}

impl BollardDocker {
    async fn observe_networks(
        &self,
        application: &ApplicationId,
    ) -> Result<(Vec<ObservedNetwork>, HashMap<String, String>), DockerError> {
        let raw_networks = Self::map_request(
            "list networks",
            self.docker
                .list_networks(Some(ListNetworksOptionsBuilder::default().build()))
                .await,
        )?;
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
            .filter(|resource| Self::relevant(&resource.name, &resource.labels, application))
            .collect();
        Ok((networks, network_names))
    }

    async fn observe_volumes(
        &self,
        application: &ApplicationId,
    ) -> Result<Vec<ObservedVolume>, DockerError> {
        Ok(Self::map_request(
            "list volumes",
            self.docker
                .list_volumes(Some(ListVolumesOptionsBuilder::default().build()))
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
        .filter(|resource| Self::relevant(&resource.name, &resource.labels, application))
        .collect())
    }

    fn observe_services(
        raw_services: Vec<bollard::models::Service>,
        all_tasks: &[bollard::models::Task],
        network_names: &HashMap<String, String>,
        secrets: &[ObservedSecret],
        application: &ApplicationId,
    ) -> Result<Vec<super::ObservedService>, DockerError> {
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
                    .map(Self::observe_task)
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
        let logical_by_name = secrets
            .iter()
            .filter_map(|secret| {
                secret
                    .labels
                    .get(SECRET_LABEL)
                    .map(|logical| (secret.name.clone(), logical.clone()))
            })
            .collect::<BTreeMap<_, _>>();
        for service in &mut services {
            Self::apply_secret_logical_names(service, &logical_by_name);
        }
        Ok(services)
    }

    fn apply_secret_logical_names(
        service: &mut super::ObservedService,
        logical_by_name: &BTreeMap<String, String>,
    ) {
        for mount in &mut service.secrets {
            if let Some(logical) = logical_by_name.get(&mount.swarm_name) {
                mount.logical_name.clone_from(logical);
            }
        }
    }
}

#[async_trait]
impl DockerApi for BollardDocker {
    async fn ensure_swarm(&self, auto_initialize: bool) -> Result<SwarmState, DockerError> {
        let info = self
            .docker
            .info()
            .await
            .map_err(|error| DockerError::unavailable("inspect Docker Swarm state", error))?;
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
        let checked =
            self.docker.info().await.map_err(|error| {
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
    }

    async fn resolve_image(&self, reference: &str) -> Result<String, DockerError> {
        // Pulling through the Engine records RepoDigests. Stream details are intentionally
        // discarded because image-pull progress is not part of the durable API contract.
        let pull = self.docker.create_image(
            Some(
                CreateImageOptionsBuilder::default()
                    .from_image(reference)
                    .build(),
            ),
            None,
            None,
        );
        pull.try_collect::<Vec<_>>()
            .await
            .map_err(|error| DockerError::image_resolution("pull image", error))?;
        let image = self
            .docker
            .inspect_image(reference)
            .await
            .map_err(|error| DockerError::image_resolution("inspect pulled image", error))?;
        let repository = image_repository(reference)
            .ok_or(DockerError::ImageResolution("parse image repository"))?;
        image
            .repo_digests
            .unwrap_or_default()
            .into_iter()
            .find(|digest| {
                image_repository(digest).as_deref() == Some(repository.as_str())
                    && Self::valid_digest(digest)
            })
            .ok_or(DockerError::ImageResolution("find repository digest"))
    }

    // Keep the correlated resource snapshot in one boundary operation so all
    // resource IDs can be normalized before service comparisons.
    #[allow(clippy::too_many_lines)]
    async fn observe(
        &self,
        application: &ApplicationId,
    ) -> Result<ObservedApplication, DockerError> {
        let (networks, network_names) = self.observe_networks(application).await?;
        let volumes = self.observe_volumes(application).await?;
        let application_filter =
            || HashMap::from([("label", vec![format!("{APPLICATION_LABEL}={application}")])]);
        let listed_services = Self::map_request(
            "list services",
            self.docker
                .list_services(Some(
                    ListServicesOptionsBuilder::default()
                        .filters(&application_filter())
                        .status(true)
                        .build(),
                ))
                .await,
        )?;
        let service_names = listed_services
            .iter()
            .filter_map(|service| service.spec.as_ref()?.name.clone())
            .collect::<Vec<_>>();
        let mut raw_services = Vec::with_capacity(listed_services.len());
        for listed in listed_services {
            let Some(id) = listed.id.as_deref() else {
                continue;
            };
            raw_services.push(self.inspect_service_wire(id).await?);
        }
        let all_tasks = Self::map_request(
            "list tasks",
            self.docker
                .list_tasks(Some(
                    ListTasksOptionsBuilder::default()
                        .filters(&HashMap::from([("service", service_names)]))
                        .build(),
                ))
                .await,
        )?;
        let secrets = self
            .observe_secret_resources(&raw_services, application)
            .await?;
        let services = Self::observe_services(
            raw_services,
            &all_tasks,
            &network_names,
            &secrets,
            application,
        )?;
        Ok(ObservedApplication {
            networks,
            volumes,
            secrets,
            services,
        })
    }

    async fn ensure_network(&self, desired: &DesiredNetwork) -> Result<(), DockerError> {
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
    }

    async fn ensure_volume(&self, desired: &DesiredVolume) -> Result<(), DockerError> {
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
    }

    async fn ensure_secret(
        &self,
        desired: &DesiredSecret,
        plaintext: &[u8],
    ) -> Result<(), DockerError> {
        if !desired.has_valid_identity() || plaintext.is_empty() || plaintext.len() > 500 * 1024 {
            return Err(DockerError::OwnershipConflict);
        }
        let existing = Self::map_request(
            "find secret by name",
            self.docker
                .list_secrets(None::<bollard::query_parameters::ListSecretsOptions>)
                .await,
        )?
        .into_iter()
        .find(|secret| {
            secret.spec.as_ref().and_then(|spec| spec.name.as_deref())
                == Some(desired.name.as_str())
        });
        if let Some(secret) = existing {
            let labels = secret
                .spec
                .and_then(|spec| spec.labels)
                .unwrap_or_default()
                .into_iter()
                .collect::<BTreeMap<_, _>>();
            return if Self::owns_named_secret(
                &labels,
                &desired.labels,
                &desired.name,
                &desired.logical_name,
            ) {
                Ok(())
            } else {
                Err(DockerError::OwnershipConflict)
            };
        }
        let data = base64::engine::general_purpose::STANDARD.encode(plaintext);
        Self::map_request(
            "create secret",
            self.docker
                .create_secret(super::SecretSpec {
                    name: Some(desired.name.clone()),
                    labels: Some(desired.labels.clone().into_iter().collect()),
                    data: Some(data),
                    ..Default::default()
                })
                .await,
        )
        .map(|_| ())
    }

    async fn ensure_service(&self, desired: &DesiredService) -> Result<(), DockerError> {
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
        let mut spec = Self::service_spec(desired)?;
        self.resolve_service_secret_ids(desired, &mut spec).await?;
        if let Some(existing) = matches
            .into_iter()
            .find(|s| s.spec.as_ref().and_then(|s| s.name.as_deref()) == Some(&desired.name))
        {
            // List responses can omit fields needed for semantic comparison.
            // Always use the complete service inspection before observing or
            // deciding whether an update is necessary.
            let existing = self
                .inspect_service_wire(existing.id.as_deref().unwrap_or(&desired.name))
                .await?;
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
            let logical_by_name = desired
                .secrets
                .iter()
                .map(|secret| (secret.swarm_name.clone(), secret.logical_name.clone()))
                .collect::<BTreeMap<_, _>>();
            Self::apply_secret_logical_names(&mut observed, &logical_by_name);
            if observed.semantically_matches(desired) {
                return Ok(());
            }
            let version = existing
                .version
                .and_then(|v| v.index)
                .ok_or(DockerError::Request("read existing service version"))?;
            self.update_service_wire(&desired.name, version, &spec)
                .await
        } else {
            self.create_service_wire(&spec).await
        }
    }

    async fn remove_service(
        &self,
        name: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
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
    }
    async fn remove_network(
        &self,
        name: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
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
    }

    async fn remove_secret(
        &self,
        name: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        let existing = Self::map_request(
            "find secret by name",
            self.docker
                .list_secrets(None::<bollard::query_parameters::ListSecretsOptions>)
                .await,
        )?
        .into_iter()
        .find(|secret| secret.spec.as_ref().and_then(|spec| spec.name.as_deref()) == Some(name));
        let Some(secret) = existing else {
            return Ok(());
        };
        let id = secret
            .id
            .ok_or(DockerError::Request("read existing secret identity"))?;
        let labels = secret
            .spec
            .and_then(|spec| spec.labels)
            .unwrap_or_default()
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        let logical_name = labels
            .get(SECRET_LABEL)
            .ok_or(DockerError::OwnershipConflict)?;
        if !Self::owns_named_secret(&labels, ownership, name, logical_name) {
            return Err(DockerError::OwnershipConflict);
        }
        match self.docker.delete_secret(&id).await {
            Ok(())
            | Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(()),
            Err(error) => Err(DockerError::request("delete secret", error)),
        }
    }
}
