use super::{
    BollardDocker, DesiredService, DockerError, HEALTH_RETRIES, HealthCheck, HealthConfig, Limit,
    Mount, MountTypeEnum, NANO_CPUS_PER_MILLICORE, NANOSECONDS_PER_SECOND, NetworkAttachmentConfig,
    RESTART_DELAY, ResourceLimits, ServiceSpec, ServiceSpecMode, ServiceSpecModeReplicated,
    ServiceSpecUpdateConfig, ServiceSpecUpdateConfigFailureActionEnum,
    ServiceSpecUpdateConfigOrderEnum, TaskSpec, TaskSpecContainerSpec, TaskSpecResources,
    TaskSpecRestartPolicy, TaskSpecRestartPolicyConditionEnum, UPDATE_MONITOR,
};

impl BollardDocker {
    /// Builds the complete Docker service specification from desired state.
    pub(super) fn service_spec(desired: &DesiredService) -> Result<ServiceSpec, DockerError> {
        if !BollardDocker::valid_digest(&desired.image) {
            return Err(DockerError::Validation("validate digest-pinned image"));
        }
        Ok(ServiceSpec {
            name: Some(desired.name.clone()),
            labels: Some(desired.labels.clone().into_iter().collect()),
            task_template: Some(Self::task_spec(desired)?),
            mode: Some(ServiceSpecMode {
                replicated: Some(ServiceSpecModeReplicated {
                    replicas: Some(i64::from(desired.replicas)),
                }),
                ..Default::default()
            }),
            update_config: Some(BollardDocker::update_config()),
            ..Default::default()
        })
    }

    /// Builds the container, network, resource, and restart portions of a service.
    fn task_spec(desired: &DesiredService) -> Result<TaskSpec, DockerError> {
        Ok(TaskSpec {
            container_spec: Some(TaskSpecContainerSpec {
                image: Some(desired.image.clone()),
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
                            source: Some(mount.volume_name.clone()),
                            typ: Some(MountTypeEnum::VOLUME),
                            read_only: Some(mount.read_only),
                            ..Default::default()
                        })
                        .collect(),
                ),
                health_check: desired
                    .healthcheck
                    .as_ref()
                    .map(Self::health_config)
                    .transpose()?,
                ..Default::default()
            }),
            networks: Some(
                desired
                    .networks
                    .iter()
                    .map(|network| NetworkAttachmentConfig {
                        target: Some(network.clone()),
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

    /// Converts a core health check into Docker's health-check representation.
    pub(super) fn health_config(health_check: &HealthCheck) -> Result<HealthConfig, DockerError> {
        match health_check {
            HealthCheck::Command {
                command,
                interval_seconds,
                timeout_seconds,
            } => {
                let test = std::iter::once("CMD".into())
                    .chain(command.clone())
                    .collect::<Vec<_>>();
                if Self::observed_wget_health(&test, *interval_seconds, *timeout_seconds).is_some()
                {
                    return Err(DockerError::Validation("validate health check"));
                }
                Ok(HealthConfig {
                    test: Some(test),
                    interval: Some(BollardDocker::seconds_to_nanoseconds(*interval_seconds)),
                    timeout: Some(BollardDocker::seconds_to_nanoseconds(*timeout_seconds)),
                    retries: Some(HEALTH_RETRIES),
                    ..Default::default()
                })
            }
            HealthCheck::Http {
                port,
                path,
                interval_seconds,
                timeout_seconds,
            } => Ok(HealthConfig {
                test: Some(vec![
                    "CMD".into(),
                    "wget".into(),
                    "-q".into(),
                    "-T".into(),
                    timeout_seconds.to_string(),
                    "-O".into(),
                    "/dev/null".into(),
                    format!("http://127.0.0.1:{port}{path}"),
                ]),
                interval: Some(BollardDocker::seconds_to_nanoseconds(*interval_seconds)),
                timeout: Some(BollardDocker::seconds_to_nanoseconds(*timeout_seconds)),
                retries: Some(HEALTH_RETRIES),
                ..Default::default()
            }),
        }
    }

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

    pub(super) fn update_config() -> ServiceSpecUpdateConfig {
        ServiceSpecUpdateConfig {
            parallelism: Some(1),
            delay: Some(0),
            failure_action: Some(ServiceSpecUpdateConfigFailureActionEnum::PAUSE),
            monitor: Some(UPDATE_MONITOR),
            max_failure_ratio: Some(0.0),
            order: Some(ServiceSpecUpdateConfigOrderEnum::START_FIRST),
        }
    }

    pub(super) fn seconds_to_nanoseconds(seconds: u32) -> i64 {
        i64::from(seconds) * NANOSECONDS_PER_SECOND
    }

    pub(super) fn nonempty(values: &[String]) -> Option<Vec<String>> {
        (!values.is_empty()).then(|| values.to_vec())
    }
}
