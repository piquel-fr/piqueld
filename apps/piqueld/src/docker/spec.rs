use super::{
    BollardDocker, DesiredService, DockerError, HealthCheck, HealthConfig, INGRESS_PROXIES_ENV,
    Limit, Mount, MountTypeEnum, NANO_CPUS_PER_MILLICORE, NANOSECONDS_PER_SECOND,
    NetworkAttachmentConfig, RESTART_DELAY, ResourceLimits, ServiceSpec, ServiceSpecMode,
    ServiceSpecModeReplicated, ServiceSpecUpdateConfig, ServiceSpecUpdateConfigFailureActionEnum,
    ServiceSpecUpdateConfigOrderEnum, TaskSpec, TaskSpecContainerSpec, TaskSpecResources,
    TaskSpecRestartPolicy, TaskSpecRestartPolicyConditionEnum,
};
use piqueld_core::manifest::{HealthExecution, RolloutOrder, RolloutPolicy};
use piqueld_core::resource::TELEMETRY_TARGET;

/// Opens a Go template action in the fields Swarm expands.
const SWARM_OPEN: &str = "{{";
/// A Go template action that prints [`SWARM_OPEN`].
const SWARM_ESCAPED_OPEN: &str = r#"{{"{{"}}"#;

impl BollardDocker {
    /// Builds the service specification pinned to `node_id`, then resolves
    /// the inputs Docker assigns: each pinned secret becomes its immutable
    /// Docker secret ID reference, each file and, for services whose health
    /// check runs `piqueld-probe`, the probe mount their Swarm configs,
    /// published on first use, and
    /// services attached to the application's ingress network get
    /// `INGRESS_PROXIES_ENV` set to that network's subnets.
    ///
    /// Fails when a secret version is missing, when a file or the health probe
    /// cannot be published, or when the ingress network is missing or still has no
    /// subnet after bounded inspection retries, since Swarm assigns subnets
    /// after creating the network.
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
        let mut configs = self.file_references(desired).await?;
        if desired
            .healthcheck
            .as_ref()
            .is_some_and(|health| health.execution().runs_probe())
        {
            configs.push(self.health_probe_reference().await?);
        }
        if !configs.is_empty() {
            container.configs = Some(configs);
        }
        if let Some(network) = desired.ingress_network() {
            let proxies = self
                .inspect_network_until(network.as_str(), |network| {
                    Self::ingress_proxies(network).is_some()
                })
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
        Ok(ServiceSpec {
            name: Some(desired.name.to_string()),
            labels: Some(desired.labels.clone().into_iter().collect()),
            task_template: Some(Self::task_spec(desired, node_id)?),
            mode: Some(ServiceSpecMode {
                replicated: Some(ServiceSpecModeReplicated {
                    replicas: Some(i64::from(desired.replicas)),
                }),
                ..Default::default()
            }),
            update_config: Some(Self::update_config(desired.rollout_policy())),
            ..Default::default()
        })
    }

    /// Builds the container, network, resource, placement, and restart
    /// portions of a service. A telemetry receiver also bind-mounts its
    /// socket directory and runs as its user.
    fn task_spec(desired: &DesiredService, node_id: &str) -> Result<TaskSpec, DockerError> {
        Ok(TaskSpec {
            placement: Some(bollard::models::TaskSpecPlacement {
                constraints: Some(vec![Self::placement_constraint(node_id)]),
                ..Default::default()
            }),
            container_spec: Some(TaskSpecContainerSpec {
                image: Some(desired.image.to_string()),
                command: Self::nonempty(&desired.command),
                args: Self::nonempty(&desired.arguments),
                env: Some(
                    desired
                        .environment
                        .iter()
                        .map(|(key, value)| format!("{key}={}", Self::swarm_literal(value)))
                        .collect(),
                ),
                mounts: Some(
                    desired
                        .mounts
                        .iter()
                        .map(|mount| Mount {
                            target: Some(Self::swarm_literal(&mount.target)),
                            source: Some(mount.volume_name.to_string()),
                            typ: Some(MountTypeEnum::VOLUME),
                            read_only: Some(mount.read_only),
                            ..Default::default()
                        })
                        .chain(desired.telemetry.as_ref().map(|telemetry| Mount {
                            target: Some(TELEMETRY_TARGET.into()),
                            source: Some(telemetry.source.clone()),
                            typ: Some(MountTypeEnum::BIND),
                            read_only: Some(false),
                            ..Default::default()
                        }))
                        .collect(),
                ),
                // A receiver runs as the daemon, so the daemon may connect to
                // the socket it creates.
                user: desired
                    .telemetry
                    .as_ref()
                    .map(|telemetry| telemetry.user.clone()),
                health_check: desired.healthcheck.as_ref().map(Self::health_config),
                ..Default::default()
            }),
            networks: Some(
                desired
                    .network_attachments()
                    .into_iter()
                    .map(|attachment| NetworkAttachmentConfig {
                        target: Some(attachment.network),
                        aliases: Self::nonempty(&attachment.aliases),
                        ..Default::default()
                    })
                    .collect(),
            ),
            resources: Self::task_resources(desired.resources.as_ref())?,
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
    /// Both variants run as a direct `CMD` vector; HTTP checks run the
    /// `piqueld-probe` config mounted by `runtime_service_spec`, via
    /// `HealthCheck::execution`.
    ///
    /// ```text
    /// Http { port: 8080, path: "/health", timeout_seconds: 3, .. }
    ///   → ["CMD","/run/piqueld/probe","http","--port","8080","--path","/health","--timeout","3"]
    /// ```
    pub(super) fn health_config(health_check: &HealthCheck) -> HealthConfig {
        let HealthExecution { command, timing } = health_check.execution();
        HealthConfig {
            test: Some(std::iter::once("CMD".into()).chain(command).collect()),
            interval: Some(Self::seconds_to_nanoseconds(timing.interval_seconds)),
            timeout: Some(Self::seconds_to_nanoseconds(timing.timeout_seconds)),
            retries: Some(timing.retries.into()),
            // Docker omits a zero start period, so none is written either.
            start_period: (timing.start_period_seconds > 0)
                .then(|| Self::seconds_to_nanoseconds(timing.start_period_seconds)),
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

    /// Returns the update configuration for `rollout`: one task at a time in
    /// its order, pausing on failure after its monitor window.
    pub(super) fn update_config(rollout: RolloutPolicy) -> ServiceSpecUpdateConfig {
        ServiceSpecUpdateConfig {
            parallelism: Some(1),
            delay: Some(0),
            failure_action: Some(ServiceSpecUpdateConfigFailureActionEnum::PAUSE),
            monitor: Some(Self::seconds_to_nanoseconds(rollout.monitor_seconds)),
            max_failure_ratio: Some(0.0),
            order: Some(match rollout.order {
                RolloutOrder::StopFirst => ServiceSpecUpdateConfigOrderEnum::STOP_FIRST,
                RolloutOrder::StartFirst => ServiceSpecUpdateConfigOrderEnum::START_FIRST,
            }),
        }
    }

    /// Converts whole seconds into Docker's nanosecond durations.
    pub(super) fn seconds_to_nanoseconds(seconds: u32) -> i64 {
        i64::from(seconds) * NANOSECONDS_PER_SECOND
    }

    /// `value` written so Swarm passes it on as is. Swarm expands Go
    /// templates in environment values and mount fields when it creates each
    /// task, so every `{{` is written as the action [`SWARM_ESCAPED_OPEN`],
    /// which prints `{{`. [`Self::swarm_text`] reads it back.
    ///
    /// ```text
    /// {{.Service.Name}} -> {{"{{"}}.Service.Name}}
    /// ```
    pub(super) fn swarm_literal(value: &str) -> String {
        value.replace(SWARM_OPEN, SWARM_ESCAPED_OPEN)
    }

    /// The value [`Self::swarm_literal`] wrote as `text`.
    pub(super) fn swarm_text(text: &str) -> String {
        text.replace(SWARM_ESCAPED_OPEN, SWARM_OPEN)
    }

    /// Whether [`Self::swarm_literal`] wrote `text`. Other text, such as a
    /// raw `{{` written before values were escaped, is expanded by Swarm.
    pub(super) fn is_swarm_literal(text: &str) -> bool {
        Self::swarm_literal(&Self::swarm_text(text)) == text
    }

    /// Maps an empty list to `None` so the image's defaults stay in effect.
    pub(super) fn nonempty(values: &[String]) -> Option<Vec<String>> {
        (!values.is_empty()).then(|| values.to_vec())
    }
}
