//! Owned Traefik ingress infrastructure.
//!
//! The controller owns only the shared overlay and one tightly constrained
//! Traefik service. Public cloudflared/origin wiring remains outside piqueld.

use async_trait::async_trait;
use bollard::{
    Docker,
    models::{
        EndpointPortConfig, EndpointPortConfigProtocolEnum, EndpointPortConfigPublishModeEnum,
        EndpointSpec, EndpointSpecModeEnum, Mount, MountTypeEnum, NetworkAttachmentConfig,
        NetworkCreateRequest, ServiceSpec, ServiceSpecMode, ServiceSpecModeReplicated, TaskSpec,
        TaskSpecContainerSpec, TaskSpecPlacement, TaskSpecRestartPolicy,
        TaskSpecRestartPolicyConditionEnum,
    },
    query_parameters::{
        ListNetworksOptionsBuilder, ListServicesOptionsBuilder, UpdateServiceOptionsBuilder,
    },
};
use piqueld_core::resource::{INSTANCE_LABEL, InstanceId, MANAGED_LABEL};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

/// Name of the shared, attachable application ingress overlay.
pub const INGRESS_NETWORK: &str = "piqueld-ingress";
/// Stable name of the internally managed Traefik Swarm service.
pub const TRAEFIK_SERVICE: &str = "piqueld-traefik";
/// Ownership role label used only for internal infrastructure.
pub const INFRASTRUCTURE_LABEL: &str = "io.piqueld.infrastructure";
const TRAEFIK_PORT: i64 = 8080;

#[derive(Clone, Debug, Eq, PartialEq)]
/// Complete host-derived desired state for managed ingress.
pub struct IngressSpec {
    /// Control-plane instance that exclusively owns the resources.
    pub instance_id: InstanceId,
    /// Immutable Traefik image reference.
    pub image: String,
    /// Optional explicit host publication for the HTTP origin.
    pub published_port: Option<u16>,
    /// Host Docker socket mounted read-only at Traefik's fixed endpoint.
    pub docker_socket: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Readiness visible to routed deployment and status paths.
pub enum InfrastructureState {
    /// The overlay and Traefik service are ready.
    Ready,
    /// Infrastructure is incomplete or has not converged.
    Degraded {
        /// Sanitized reason readiness is not satisfied.
        diagnostic: String,
    },
}

#[derive(Clone, Copy, Debug, thiserror::Error, Eq, PartialEq)]
/// Sanitized ingress controller failures.
pub enum IngressError {
    /// A same-named resource is not owned in the expected role.
    #[error("ingress infrastructure has an ownership conflict")]
    OwnershipConflict,
    /// Desired or observed settings would violate the security contract.
    #[error("ingress infrastructure configuration is unsafe")]
    UnsafeConfiguration,
    /// Docker rejected or could not complete a request.
    #[error("ingress infrastructure request failed")]
    Request,
}

#[async_trait]
/// Mockable idempotent ingress-management boundary.
pub trait IngressApi: Send + Sync + 'static {
    /// Creates or repairs owned resources and waits for bounded convergence.
    async fn ensure(&self, spec: &IngressSpec) -> Result<InfrastructureState, IngressError>;
    /// Observes readiness without creating missing resources.
    async fn status(&self, spec: &IngressSpec) -> Result<InfrastructureState, IngressError>;
}

#[derive(Clone)]
/// Bollard-backed controller for the shared overlay and Traefik service.
pub struct TraefikController {
    docker: Arc<Docker>,
    ensure_lock: Arc<tokio::sync::Mutex<()>>,
}

impl TraefikController {
    /// Builds a controller around the daemon's shared Docker connection.
    #[must_use]
    pub fn new(docker: Docker) -> Self {
        Self {
            docker: Arc::new(docker),
            ensure_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    async fn existing(&self) -> Result<Option<bollard::models::Service>, IngressError> {
        self.docker
            .list_services(Some(
                ListServicesOptionsBuilder::default()
                    .filters(&HashMap::from([("name", vec![TRAEFIK_SERVICE.to_owned()])]))
                    .status(true)
                    .build(),
            ))
            .await
            .map_err(|_| IngressError::Request)
            .map(|services| {
                services.into_iter().find(|service| {
                    service.spec.as_ref().and_then(|spec| spec.name.as_deref())
                        == Some(TRAEFIK_SERVICE)
                })
            })
    }

    async fn network_ready(
        &self,
        instance: &InstanceId,
        create: bool,
    ) -> Result<Option<String>, IngressError> {
        let networks = self
            .docker
            .list_networks(Some(
                ListNetworksOptionsBuilder::default()
                    .filters(&HashMap::from([("name", vec![INGRESS_NETWORK.to_owned()])]))
                    .build(),
            ))
            .await
            .map_err(|_| IngressError::Request)?;
        if let Some(network) = networks
            .into_iter()
            .find(|network| network.name.as_deref() == Some(INGRESS_NETWORK))
        {
            let labels = network.labels.unwrap_or_default();
            if labels.len() != 2
                || labels.get(MANAGED_LABEL).map(String::as_str) != Some("true")
                || labels.get(INSTANCE_LABEL).map(String::as_str) != Some(instance.as_str())
            {
                return Err(IngressError::OwnershipConflict);
            }
            return if network.driver.as_deref() == Some("overlay")
                && network.attachable == Some(true)
                && network.ingress != Some(true)
                && network.internal != Some(true)
            {
                network.id.map(Some).ok_or(IngressError::Request)
            } else {
                Err(IngressError::UnsafeConfiguration)
            };
        }
        if !create {
            return Ok(None);
        }
        let created = self
            .docker
            .create_network(NetworkCreateRequest {
                name: INGRESS_NETWORK.to_owned(),
                driver: Some("overlay".into()),
                internal: Some(false),
                attachable: Some(true),
                ingress: Some(false),
                labels: Some(HashMap::from([
                    (MANAGED_LABEL.into(), "true".into()),
                    (INSTANCE_LABEL.into(), instance.to_string()),
                ])),
                ..Default::default()
            })
            .await
            .map_err(|_| IngressError::Request)?;
        Ok(Some(created.id))
    }
}

#[async_trait]
impl IngressApi for TraefikController {
    async fn ensure(&self, desired: &IngressSpec) -> Result<InfrastructureState, IngressError> {
        let _guard = self.ensure_lock.lock().await;
        if !pinned_image(&desired.image)
            || !desired.docker_socket.is_absolute()
            || desired.docker_socket.file_name().is_none()
        {
            return Err(IngressError::UnsafeConfiguration);
        }
        let network_id = self
            .network_ready(&desired.instance_id, true)
            .await?
            .ok_or(IngressError::Request)?;
        let spec = service_spec(desired, &network_id);
        if let Some(existing) = self.existing().await? {
            let labels = existing
                .spec
                .as_ref()
                .and_then(|spec| spec.labels.as_ref())
                .cloned()
                .unwrap_or_default();
            if !owned(&labels, &desired.instance_id) {
                return Err(IngressError::OwnershipConflict);
            }
            if !service_matches(existing.spec.as_ref(), &spec) {
                let version = existing
                    .version
                    .and_then(|version| version.index)
                    .ok_or(IngressError::Request)?;
                self.docker
                    .update_service(
                        TRAEFIK_SERVICE,
                        spec,
                        UpdateServiceOptionsBuilder::default()
                            .version(i32::try_from(version).map_err(|_| IngressError::Request)?)
                            .build(),
                        None,
                    )
                    .await
                    .map_err(|_| IngressError::Request)?;
            }
        } else {
            self.docker
                .create_service(spec, None)
                .await
                .map_err(|_| IngressError::Request)?;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_mins(1);
        loop {
            let status = self.status(desired).await?;
            if status == InfrastructureState::Ready || tokio::time::Instant::now() >= deadline {
                return Ok(status);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn status(&self, desired: &IngressSpec) -> Result<InfrastructureState, IngressError> {
        let Some(network_id) = self.network_ready(&desired.instance_id, false).await? else {
            return Ok(InfrastructureState::Degraded {
                diagnostic: "ingress overlay network is missing".into(),
            });
        };
        let Some(existing) = self.existing().await? else {
            return Ok(InfrastructureState::Degraded {
                diagnostic: "Traefik service is missing".into(),
            });
        };
        let labels = existing
            .spec
            .as_ref()
            .and_then(|spec| spec.labels.as_ref())
            .cloned()
            .unwrap_or_default();
        if !owned(&labels, &desired.instance_id) {
            return Err(IngressError::OwnershipConflict);
        }
        if !service_matches(existing.spec.as_ref(), &service_spec(desired, &network_id)) {
            return Ok(InfrastructureState::Degraded {
                diagnostic: "Traefik service configuration has not converged".into(),
            });
        }
        let service_status = existing.service_status.unwrap_or_default();
        let desired_tasks = service_status.desired_tasks.unwrap_or(1);
        let running = service_status.running_tasks.unwrap_or(0);
        if desired_tasks == 1 && running == 1 {
            Ok(InfrastructureState::Ready)
        } else {
            Ok(InfrastructureState::Degraded {
                diagnostic: format!("Traefik has {running}/{desired_tasks} running tasks"),
            })
        }
    }
}

fn service_matches(observed: Option<&ServiceSpec>, desired: &ServiceSpec) -> bool {
    let Some(observed) = observed else {
        return false;
    };
    let Some(observed_task) = observed.task_template.as_ref() else {
        return false;
    };
    let Some(desired_task) = desired.task_template.as_ref() else {
        return false;
    };
    observed.name == desired.name
        && observed.labels == desired.labels
        && observed.mode == desired.mode
        && observed.endpoint_spec == desired.endpoint_spec
        && observed_task.networks == desired_task.networks
        && restart_policy_matches(
            observed_task.restart_policy.as_ref(),
            desired_task.restart_policy.as_ref(),
        )
        && placement_matches(
            observed_task.placement.as_ref(),
            desired_task.placement.as_ref(),
        )
        && observed_task
            .container_spec
            .as_ref()
            .zip(desired_task.container_spec.as_ref())
            .is_some_and(|(observed, desired)| {
                observed.image == desired.image
                    && empty_vec_option_matches(observed.args.as_deref(), desired.args.as_deref())
                    && empty_vec_option_matches(
                        observed.mounts.as_deref(),
                        desired.mounts.as_deref(),
                    )
                    && empty_vec_option_matches(observed.env.as_deref(), desired.env.as_deref())
                    && observed.read_only.unwrap_or(false) == desired.read_only.unwrap_or(false)
            })
}

fn restart_policy_matches(
    observed: Option<&TaskSpecRestartPolicy>,
    desired: Option<&TaskSpecRestartPolicy>,
) -> bool {
    let (Some(observed), Some(desired)) = (observed, desired) else {
        return observed == desired;
    };
    observed.condition == desired.condition
        && observed.delay == desired.delay
        && observed.max_attempts.unwrap_or(0) == desired.max_attempts.unwrap_or(0)
        && observed.window.unwrap_or(0) == desired.window.unwrap_or(0)
}

fn placement_matches(
    observed: Option<&TaskSpecPlacement>,
    desired: Option<&TaskSpecPlacement>,
) -> bool {
    let (Some(observed), Some(desired)) = (observed, desired) else {
        return default_option_matches(observed, desired);
    };
    empty_vec_option_matches(
        observed.constraints.as_deref(),
        desired.constraints.as_deref(),
    ) && empty_vec_option_matches(
        observed.preferences.as_deref(),
        desired.preferences.as_deref(),
    ) && observed.max_replicas.unwrap_or(0) == desired.max_replicas.unwrap_or(0)
        && empty_vec_option_matches(observed.platforms.as_deref(), desired.platforms.as_deref())
}

fn default_option_matches<T: Default + PartialEq>(
    observed: Option<&T>,
    desired: Option<&T>,
) -> bool {
    match (observed, desired) {
        (Some(observed), Some(desired)) => observed == desired,
        (Some(observed), None) => observed == &T::default(),
        (None, Some(desired)) => desired == &T::default(),
        (None, None) => true,
    }
}

fn empty_vec_option_matches<T: PartialEq>(observed: Option<&[T]>, desired: Option<&[T]>) -> bool {
    match (observed, desired) {
        (None | Some([]), None | Some([])) => true,
        (Some(observed), Some(desired)) => observed == desired,
        _ => false,
    }
}

fn owned(labels: &HashMap<String, String>, instance: &InstanceId) -> bool {
    labels.len() == 3
        && labels.get(MANAGED_LABEL).map(String::as_str) == Some("true")
        && labels.get(INSTANCE_LABEL).map(String::as_str) == Some(instance.as_str())
        && labels.get(INFRASTRUCTURE_LABEL).map(String::as_str) == Some("traefik")
}

fn pinned_image(image: &str) -> bool {
    image.split_once("@sha256:").is_some_and(|(name, digest)| {
        !name.is_empty()
            && !name.contains('@')
            && digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn service_spec(desired: &IngressSpec, network_target: &str) -> ServiceSpec {
    let endpoint_spec = desired.published_port.map(|port| EndpointSpec {
        mode: Some(EndpointSpecModeEnum::VIP),
        ports: Some(vec![EndpointPortConfig {
            name: Some("web".into()),
            protocol: Some(EndpointPortConfigProtocolEnum::TCP),
            target_port: Some(TRAEFIK_PORT),
            published_port: Some(i64::from(port)),
            publish_mode: Some(EndpointPortConfigPublishModeEnum::HOST),
        }]),
    });
    ServiceSpec {
        name: Some(TRAEFIK_SERVICE.into()),
        labels: Some(HashMap::from([
            (MANAGED_LABEL.into(), "true".into()),
            (INSTANCE_LABEL.into(), desired.instance_id.to_string()),
            (INFRASTRUCTURE_LABEL.into(), "traefik".into()),
        ])),
        task_template: Some(TaskSpec {
            container_spec: Some(TaskSpecContainerSpec {
                image: Some(desired.image.clone()),
                args: Some(vec![
                    "--api=false".into(),
                    "--api.dashboard=false".into(),
                    "--entrypoints.web.address=:8080".into(),
                    "--providers.swarm=true".into(),
                    "--providers.swarm.exposedbydefault=false".into(),
                    format!("--providers.swarm.network={INGRESS_NETWORK}"),
                    "--providers.swarm.endpoint=unix:///var/run/docker.sock".into(),
                    "--providers.swarm.watch=true".into(),
                    format!(
                        "--providers.swarm.constraints=Label(`{MANAGED_LABEL}`,`true`) && Label(`{INSTANCE_LABEL}`,`{}`)",
                        desired.instance_id
                    ),
                ]),
                mounts: Some(vec![Mount {
                    target: Some("/var/run/docker.sock".into()),
                    source: Some(desired.docker_socket.to_string_lossy().into_owned()),
                    typ: Some(MountTypeEnum::BIND),
                    read_only: Some(true),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            networks: Some(vec![NetworkAttachmentConfig {
                target: Some(network_target.into()),
                ..Default::default()
            }]),
            restart_policy: Some(TaskSpecRestartPolicy {
                condition: Some(TaskSpecRestartPolicyConditionEnum::ON_FAILURE),
                delay: Some(2_000_000_000),
                ..Default::default()
            }),
            placement: Some(bollard::models::TaskSpecPlacement {
                constraints: Some(vec!["node.role==manager".into()]),
                ..Default::default()
            }),
            ..Default::default()
        }),
        mode: Some(ServiceSpecMode {
            replicated: Some(ServiceSpecModeReplicated { replicas: Some(1) }),
            ..Default::default()
        }),
        endpoint_spec,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desired(port: Option<u16>) -> IngressSpec {
        IngressSpec {
            instance_id: InstanceId::parse("home-1").unwrap(),
            image: "traefik:v3.5.0@sha256:4e7175cfe19be83c6b928cae49dde2f2788fb307189a4dc9550b67acf30c11a5".into(),
            published_port: port,
            docker_socket: PathBuf::from("/var/run/docker.sock"),
        }
    }

    #[test]
    fn secure_configuration_disables_admin_and_pins_the_socket() {
        let spec = service_spec(&desired(None), "network-id");
        assert!(spec.endpoint_spec.is_none());
        let task = spec.task_template.unwrap();
        let container = task.container_spec.unwrap();
        let args = container.args.unwrap();
        assert!(args.contains(&"--api=false".to_owned()));
        assert!(args.contains(&"--api.dashboard=false".to_owned()));
        assert!(args.contains(&"--providers.swarm.exposedbydefault=false".to_owned()));
        assert_eq!(container.mounts.as_ref().unwrap()[0].read_only, Some(true));
    }

    #[test]
    fn host_publication_is_explicit() {
        let spec = service_spec(&desired(Some(8080)), "network-id");
        let port = &spec.endpoint_spec.unwrap().ports.unwrap()[0];
        assert_eq!(port.published_port, Some(8080));
        assert_eq!(
            port.publish_mode,
            Some(EndpointPortConfigPublishModeEnum::HOST)
        );
    }

    #[test]
    fn image_must_be_digest_pinned() {
        assert!(pinned_image(&desired(None).image));
        assert!(!pinned_image("traefik:v3.5.0"));
    }

    #[test]
    fn daemon_default_canonicalization_does_not_trigger_ingress_drift() {
        let wanted = service_spec(&desired(None), "network-id");
        let mut observed = wanted.clone();
        let task = observed.task_template.as_mut().unwrap();
        let restart = task.restart_policy.as_mut().unwrap();
        restart.max_attempts = Some(0);
        restart.window = Some(0);
        let placement = task.placement.as_mut().unwrap();
        placement.preferences = Some(Vec::new());
        placement.max_replicas = Some(0);
        placement.platforms = Some(Vec::new());
        assert!(service_matches(Some(&observed), &wanted));
    }
}
