use super::{
    BollardDocker, DesiredService, DockerError, HEALTH_RETRIES, HealthCheck, HealthConfig,
    INGRESS_PROXIES_ENV, Limit, Mount, MountTypeEnum, NANO_CPUS_PER_MILLICORE,
    NANOSECONDS_PER_SECOND, NetworkAttachmentConfig, RESTART_DELAY, ResourceLimits, ServiceSpec,
    ServiceSpecMode, ServiceSpecModeReplicated, ServiceSpecUpdateConfig,
    ServiceSpecUpdateConfigFailureActionEnum, ServiceSpecUpdateConfigOrderEnum, TaskSpec,
    TaskSpecContainerSpec, TaskSpecResources, TaskSpecRestartPolicy,
    TaskSpecRestartPolicyConditionEnum, UPDATE_MONITOR,
};

impl BollardDocker {
    /// Builds the service specification pinned to `node_id`, then resolves
    /// the inputs Docker assigns: each pinned secret becomes its immutable
    /// Docker secret ID reference, and services attached to the application's
    /// ingress network get `INGRESS_PROXIES_ENV` set to that network's subnets.
    ///
    /// Fails when a secret version is missing, or when the ingress network is
    /// missing or has no subnet.
    pub(super) async fn runtime_service_spec(
        &self,
        desired: &DesiredService,
        node_id: &str,
    ) -> Result<ServiceSpec, DockerError> {
        let mut spec = Self::service_spec(desired, node_id)?;
        let container = spec
            .task_template
            .as_mut()
            .expect("task spec")
            .container_spec
            .as_mut()
            .expect("container spec");
        if !desired.secrets.is_empty() {
            container.secrets = Some(self.secret_references(desired).await?);
        }
        if let Some(network) = desired.ingress_network() {
            let proxies = self
                .inspect_network_complete(network.as_str())
                .await?
                .as_ref()
                .and_then(Self::ingress_proxies)
                .ok_or(DockerError::Request("read ingress network subnet"))?;
            container
                .env
                .get_or_insert_default()
                .push(format!("{INGRESS_PROXIES_ENV}={proxies}"));
        }
        Ok(spec)
    }

    /// Lists a network's Docker-assigned subnets as comma-separated CIDRs, or
    /// `None` when it has none.
    ///
    /// ```text
    /// 10.0.1.0/24,fd00:1::/64
    /// ```
    pub(super) fn ingress_proxies(network: &bollard::models::Network) -> Option<String> {
        let subnets = network
            .ipam
            .as_ref()?
            .config
            .iter()
            .flatten()
            .filter_map(|config| config.subnet.as_deref().filter(|subnet| !subnet.is_empty()))
            .collect::<Vec<_>>();
        (!subnets.is_empty()).then(|| subnets.join(","))
    }

    /// Builds the complete Docker service specification from desired state.
    pub(super) fn service_spec(
        desired: &DesiredService,
        node_id: &str,
    ) -> Result<ServiceSpec, DockerError> {
        let task_template = Self::task_spec(desired, node_id)?;
        let update_config = Self::update_config(
            task_template
                .container_spec
                .as_ref()
                .expect("container spec"),
        );
        Ok(ServiceSpec {
            name: Some(desired.name.to_string()),
            labels: Some(desired.labels.clone().into_iter().collect()),
            task_template: Some(task_template),
            mode: Some(ServiceSpecMode {
                replicated: Some(ServiceSpecModeReplicated {
                    replicas: Some(i64::from(desired.replicas)),
                }),
                ..Default::default()
            }),
            update_config: Some(update_config),
            ..Default::default()
        })
    }

    /// Builds the container, network, resource, placement, and restart portions of a service.
    fn task_spec(desired: &DesiredService, node_id: &str) -> Result<TaskSpec, DockerError> {
        Ok(TaskSpec {
            placement: Some(bollard::models::TaskSpecPlacement {
                constraints: Some(vec![Self::placement_constraint(node_id)]),
                ..Default::default()
            }),
            container_spec: Some(TaskSpecContainerSpec {
                image: Some(desired.image.to_string()),
                command: BollardDocker::nonempty(&desired.command),
                args: BollardDocker::nonempty(&desired.arguments),
                env: Some(
                    desired
                        .environment
                        .iter()
                        .map(|(key, value)| format!("{key}={value}"))
                        .collect(),
                ),
                mounts: Some(
                    desired
                        .mounts
                        .iter()
                        .map(|mount| Mount {
                            target: Some(mount.target.clone()),
                            source: Some(mount.volume_name.to_string()),
                            typ: Some(MountTypeEnum::VOLUME),
                            read_only: Some(mount.read_only),
                            ..Default::default()
                        })
                        .collect(),
                ),
                health_check: desired.healthcheck.as_ref().map(Self::health_config),
                ..Default::default()
            }),
            networks: Some(
                desired
                    .network_attachments()
                    .into_iter()
                    .map(|attachment| NetworkAttachmentConfig {
                        target: Some(attachment.network),
                        aliases: BollardDocker::nonempty(&attachment.aliases),
                        ..Default::default()
                    })
                    .collect(),
            ),
            resources: BollardDocker::task_resources(desired.resources.as_ref())?,
            restart_policy: Some(TaskSpecRestartPolicy {
                condition: Some(TaskSpecRestartPolicyConditionEnum::ANY),
                delay: Some(RESTART_DELAY),
                max_attempts: None,
                window: None,
            }),
            ..Default::default()
        })
    }

    /// Returns the constraint pinning tasks to `node_id`, which keeps local images
    /// and volumes on the daemon's node even if another node joins.
    pub(super) fn placement_constraint(node_id: &str) -> String {
        format!("node.id == {node_id}")
    }

    /// Converts a core health check into Docker's health-check representation.
    ///
    /// Both variants run as a direct `CMD` vector; HTTP checks become the
    /// equivalent `wget` probe via `HealthCheck::execution`.
    ///
    /// ```text
    /// Http { port: 8080, path: "/health", timeout_seconds: 3, .. }
    ///   → ["CMD","wget","-q","-T","3","-O","/dev/null","http://127.0.0.1:8080/health"]
    /// ```
    pub(super) fn health_config(health_check: &HealthCheck) -> HealthConfig {
        let execution = health_check.execution();
        HealthConfig {
            test: Some(
                std::iter::once("CMD".into())
                    .chain(execution.command)
                    .collect(),
            ),
            interval: Some(Self::seconds_to_nanoseconds(execution.interval_seconds)),
            timeout: Some(Self::seconds_to_nanoseconds(execution.timeout_seconds)),
            retries: Some(HEALTH_RETRIES),
            ..Default::default()
        }
    }

    /// Converts core resource limits into Docker task limits, leaving
    /// reservations unset. Fails when the memory limit exceeds `i64::MAX`.
    pub(super) fn task_resources(
        resources: Option<&ResourceLimits>,
    ) -> Result<Option<TaskSpecResources>, DockerError> {
        let Some(limits) = resources else {
            return Ok(None);
        };
        let memory_bytes = limits
            .memory_bytes
            .map(i64::try_from)
            .transpose()
            .map_err(|_| DockerError::Validation("validate memory limit"))?;
        Ok(Some(TaskSpecResources {
            limits: Some(Limit {
                nano_cpus: limits
                    .cpu_millis
                    .map(|millis| i64::from(millis) * NANO_CPUS_PER_MILLICORE),
                memory_bytes,
                pids: None,
            }),
            reservations: None,
        }))
    }

    /// Returns the rollout policy for `container`: one task at a time in
    /// `update_order`, pausing on failure after the monitor window.
    pub(super) fn update_config(container: &TaskSpecContainerSpec) -> ServiceSpecUpdateConfig {
        ServiceSpecUpdateConfig {
            parallelism: Some(1),
            delay: Some(0),
            failure_action: Some(ServiceSpecUpdateConfigFailureActionEnum::PAUSE),
            monitor: Some(UPDATE_MONITOR),
            max_failure_ratio: Some(0.0),
            order: Some(Self::update_order(container)),
        }
    }

    /// Returns stop-first when `container` mounts any volume read-write, so two
    /// tasks never share a writable data directory (e.g. `PostgreSQL`), at the
    /// cost of a short downtime per rollout. Every other service starts its
    /// replacement first. piqueld only authors named-volume mounts.
    pub(super) fn update_order(
        container: &TaskSpecContainerSpec,
    ) -> ServiceSpecUpdateConfigOrderEnum {
        if container
            .mounts
            .iter()
            .flatten()
            .any(|mount| !mount.read_only.unwrap_or(false))
        {
            ServiceSpecUpdateConfigOrderEnum::STOP_FIRST
        } else {
            ServiceSpecUpdateConfigOrderEnum::START_FIRST
        }
    }

    /// Converts whole seconds into Docker's nanosecond durations.
    pub(super) fn seconds_to_nanoseconds(seconds: u32) -> i64 {
        i64::from(seconds) * NANOSECONDS_PER_SECOND
    }

    /// Maps an empty list to `None` so the image's defaults stay in effect.
    pub(super) fn nonempty(values: &[String]) -> Option<Vec<String>> {
        (!values.is_empty()).then(|| values.to_vec())
    }
}
