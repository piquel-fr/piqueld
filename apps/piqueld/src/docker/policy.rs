use super::{
    BTreeSet, BollardDocker, HEALTH_RETRIES, HashMap, HealthCheck, HealthConfig, MountTypeEnum,
    NANOSECONDS_PER_MILLISECOND, RESTART_DELAY, ROLLBACK_MONITOR, STOP_GRACE_PERIOD, ServiceSpec,
    ServiceSpecRollbackConfig, ServiceSpecRollbackConfigFailureActionEnum,
    ServiceSpecRollbackConfigOrderEnum, ServiceSpecUpdateConfigFailureActionEnum,
    ServiceSpecUpdateConfigOrderEnum, TaskSpec, TaskSpecContainerSpec,
    TaskSpecRestartPolicyConditionEnum, UPDATE_MONITOR,
};

/// The runtime policy emitted by the desired-service Docker specification builder.
///
/// Docker may add harmless defaults while creating a service, so this policy
/// accepts those defaults but rejects unsupported attachments and weakened
/// update, restart, health-check, or resource settings as drift.
pub(super) struct ServiceRuntimePolicy;

impl ServiceRuntimePolicy {
    pub(super) fn matches(spec: &ServiceSpec) -> bool {
        let Some(task) = spec.task_template.as_ref() else {
            return false;
        };
        let Some(container) = task.container_spec.as_ref() else {
            return false;
        };
        Self::restart_policy(task)
            && Self::update_policy(spec)
            && Self::replicated_mode(spec)
            && Self::mounts(container)
            && Self::environment(container)
            && Self::networks(task)
            && Self::health(container)
            && Self::container_configuration(container)
            && Self::task_configuration(spec, task)
    }

    fn restart_policy(task: &TaskSpec) -> bool {
        task.restart_policy.as_ref().is_some_and(|restart| {
            restart.condition == Some(TaskSpecRestartPolicyConditionEnum::ANY)
                && restart.delay == Some(RESTART_DELAY)
                && restart.max_attempts.is_none_or(|value| value == 0)
                && restart.window.is_none_or(|value| value == 0)
        })
    }

    fn update_policy(spec: &ServiceSpec) -> bool {
        spec.update_config.as_ref().is_some_and(|update| {
            update.parallelism == Some(1)
                && update.delay.is_none_or(|value| value == 0)
                && update.failure_action == Some(ServiceSpecUpdateConfigFailureActionEnum::PAUSE)
                && update.monitor == Some(UPDATE_MONITOR)
                && update.max_failure_ratio == Some(0.0)
                && update.order == Some(ServiceSpecUpdateConfigOrderEnum::START_FIRST)
        })
    }

    fn replicated_mode(spec: &ServiceSpec) -> bool {
        spec.mode.as_ref().is_some_and(|mode| {
            mode.replicated.is_some()
                && mode.global.is_none()
                && mode.replicated_job.is_none()
                && mode.global_job.is_none()
        })
    }

    fn mounts(container: &TaskSpecContainerSpec) -> bool {
        container.mounts.as_ref().is_none_or(|mounts| {
            mounts.iter().all(|mount| {
                mount.typ == Some(MountTypeEnum::VOLUME)
                    && mount.source.as_ref().is_some_and(|value| !value.is_empty())
                    && mount.target.as_ref().is_some_and(|value| !value.is_empty())
                    && mount.consistency.as_deref().is_none_or(str::is_empty)
                    && mount.bind_options.is_none()
                    && mount.volume_options.as_ref().is_none_or(Self::is_default)
                    && mount.image_options.is_none()
                    && mount.tmpfs_options.is_none()
            })
        })
    }

    fn environment(container: &TaskSpecContainerSpec) -> bool {
        container.env.as_ref().is_none_or(|environment| {
            let mut keys = BTreeSet::new();
            environment.iter().all(|entry| {
                entry
                    .split_once('=')
                    .is_some_and(|(key, _)| !key.is_empty() && keys.insert(key))
            })
        })
    }

    fn networks(task: &TaskSpec) -> bool {
        task.networks.as_ref().is_none_or(|networks| {
            let mut targets = BTreeSet::new();
            networks.iter().all(|network| {
                network
                    .target
                    .as_ref()
                    .is_some_and(|value| !value.is_empty() && targets.insert(value.as_str()))
                    && network.aliases.as_ref().is_none_or(Vec::is_empty)
                    && network.driver_opts.as_ref().is_none_or(HashMap::is_empty)
            })
        })
    }

    fn health(container: &TaskSpecContainerSpec) -> bool {
        container
            .health_check
            .as_ref()
            .is_none_or(Self::supported_health_config)
    }

    fn container_configuration(container: &TaskSpecContainerSpec) -> bool {
        container.labels.as_ref().is_none_or(HashMap::is_empty)
            && container.hostname.as_deref().is_none_or(str::is_empty)
            && container.dir.as_deref().is_none_or(str::is_empty)
            && container.user.as_deref().is_none_or(str::is_empty)
            && container.groups.as_ref().is_none_or(Vec::is_empty)
            && container.privileges.as_ref().is_none_or(Self::is_default)
            && !container.tty.unwrap_or(false)
            && !container.open_stdin.unwrap_or(false)
            && !container.read_only.unwrap_or(false)
            && container.stop_signal.as_deref().is_none_or(str::is_empty)
            && container
                .stop_grace_period
                .is_none_or(|value| value == STOP_GRACE_PERIOD)
            && container.hosts.as_ref().is_none_or(Vec::is_empty)
            && container.dns_config.as_ref().is_none_or(Self::is_default)
            && container.oom_score_adj.is_none_or(|value| value == 0)
            && container.configs.as_ref().is_none_or(Vec::is_empty)
            && container.isolation.is_none_or(|value| {
                matches!(
                    value,
                    bollard::models::TaskSpecContainerSpecIsolationEnum::EMPTY
                        | bollard::models::TaskSpecContainerSpecIsolationEnum::DEFAULT
                )
            })
            && !container.init.unwrap_or(false)
            && container.sysctls.as_ref().is_none_or(HashMap::is_empty)
            && container.capability_add.as_ref().is_none_or(Vec::is_empty)
            && container.capability_drop.as_ref().is_none_or(Vec::is_empty)
            && container.ulimits.as_ref().is_none_or(Vec::is_empty)
    }

    fn task_configuration(spec: &ServiceSpec, task: &TaskSpec) -> bool {
        task.plugin_spec.is_none()
            && task.network_attachment_spec.is_none()
            && task.force_update.is_none_or(|value| value == 0)
            && task
                .resources
                .as_ref()
                .and_then(|resources| resources.limits.as_ref())
                .is_none_or(|limits| {
                    limits.pids.is_none_or(|value| value == 0)
                        && limits.nano_cpus.is_none_or(|value| {
                            value >= 0 && value % NANOSECONDS_PER_MILLISECOND == 0
                        })
                        && limits.memory_bytes.is_none_or(|value| value >= 0)
                })
            && task
                .resources
                .as_ref()
                .and_then(|resources| resources.reservations.as_ref())
                .is_none_or(Self::is_default)
            && task.placement.as_ref().is_none_or(Self::is_default)
            && task
                .runtime
                .as_deref()
                .is_none_or(|runtime| runtime.is_empty() || runtime == "container")
            && task.log_driver.as_ref().is_none_or(Self::is_default)
            && spec
                .rollback_config
                .as_ref()
                .is_none_or(Self::supported_rollback_config)
            && spec.networks.as_ref().is_none_or(Vec::is_empty)
    }

    fn supported_health_config(health: &HealthConfig) -> bool {
        if health.retries != Some(HEALTH_RETRIES)
            || health.interval.is_none_or(|value| value <= 0)
            || health.timeout.is_none_or(|value| value <= 0)
            || health.start_period.is_some_and(|value| value != 0)
            || health.start_interval.is_some_and(|value| value != 0)
        {
            return false;
        }
        match BollardDocker::observed_health(health) {
            Some(HealthCheck::Command { ref command, .. }) => {
                health.test.as_ref()
                    == Some(
                        &std::iter::once("CMD".into())
                            .chain(command.clone())
                            .collect::<Vec<_>>(),
                    )
            }
            Some(HealthCheck::Http {
                port,
                ref path,
                timeout_seconds,
                ..
            }) => {
                health.test.as_ref()
                    == Some(&vec![
                        "CMD".into(),
                        "wget".into(),
                        "-q".into(),
                        "-T".into(),
                        timeout_seconds.to_string(),
                        "-O".into(),
                        "/dev/null".into(),
                        format!("http://127.0.0.1:{port}{path}"),
                    ])
            }
            None => false,
        }
    }

    fn supported_rollback_config(rollback: &ServiceSpecRollbackConfig) -> bool {
        rollback.parallelism == Some(1)
            && rollback.delay.is_none_or(|value| value == 0)
            && rollback.failure_action == Some(ServiceSpecRollbackConfigFailureActionEnum::PAUSE)
            && rollback.monitor == Some(ROLLBACK_MONITOR)
            && rollback.max_failure_ratio == Some(0.0)
            && rollback.order == Some(ServiceSpecRollbackConfigOrderEnum::STOP_FIRST)
    }

    fn is_default<T: Default + PartialEq>(value: &T) -> bool {
        value == &T::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::DockerError;

    #[test]
    fn generated_http_health_check_round_trips_through_observation() {
        let health_check = HealthCheck::Http {
            port: 8080,
            path: "/health".into(),
            interval_seconds: 10,
            timeout_seconds: 3,
        };
        let config = BollardDocker::health_config(&health_check).expect("valid HTTP health check");

        assert_eq!(BollardDocker::observed_health(&config), Some(health_check));
        assert!(ServiceRuntimePolicy::supported_health_config(&config));
    }

    #[test]
    fn command_health_check_rejects_reserved_wget_vector() {
        let health_check = HealthCheck::Command {
            command: vec![
                "wget".into(),
                "-q".into(),
                "-T".into(),
                "3".into(),
                "-O".into(),
                "/dev/null".into(),
                "http://127.0.0.1:8080/health".into(),
            ],
            interval_seconds: 10,
            timeout_seconds: 3,
        };

        assert!(matches!(
            BollardDocker::health_config(&health_check),
            Err(DockerError::Validation("validate health check"))
        ));
    }
}
