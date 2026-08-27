use super::{
    BTreeSet, BollardDocker, HEALTH_RETRIES, HealthCheck, HealthConfig, MountTypeEnum,
    NANO_CPUS_PER_MILLICORE, RESTART_DELAY, ServiceSpec, ServiceSpecUpdateConfigFailureActionEnum,
    ServiceSpecUpdateConfigOrderEnum, TaskSpec, TaskSpecContainerSpec,
    TaskSpecRestartPolicyConditionEnum, UPDATE_MONITOR,
};

/// The runtime policy emitted by the desired-service Docker specification builder.
///
/// This policy verifies exactly the fields piqueld authors — replication,
/// update settings, the restart condition and delay, mounts, environment,
/// network targets, health checks, and resource limits. Fields the builder
/// never sets are accepted only at known Engine defaults, so ordinary
/// engine-defaulted echo-back does not register as drift while unsupported
/// non-default values do. Security relevant settings piqueld cannot express
/// (privileged execution, Linux capabilities, sysctls, users, runtimes, log
/// drivers) are explicitly denied: their presence means out-of-band
/// modification.
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
            && Self::resource_limits(task)
            && Self::no_unsupported_service_settings(spec)
            && Self::no_security_settings(task, container)
    }

    /// Rejects security-sensitive settings piqueld never authors; an observed
    /// service carrying them was modified out of band and must be reconciled
    /// back to the authored specification.
    fn no_security_settings(task: &TaskSpec, container: &TaskSpecContainerSpec) -> bool {
        task.runtime
            .as_deref()
            .is_none_or(|runtime| runtime.is_empty() || runtime == "container")
            && task.log_driver.is_none()
            && task.plugin_spec.is_none()
            && task.network_attachment_spec.is_none()
            && task.placement.as_ref().is_none_or(|placement| {
                placement.constraints.as_ref().is_none_or(Vec::is_empty)
                    && placement.preferences.as_ref().is_none_or(Vec::is_empty)
                    && placement.max_replicas.is_none_or(|value| value == 0)
                    && placement.platforms.as_ref().is_none_or(Vec::is_empty)
            })
            && task.force_update.is_none_or(|value| value == 0)
            && container.user.is_none()
            && container.groups.as_ref().is_none_or(Vec::is_empty)
            && container.privileges.is_none()
            && container
                .labels
                .as_ref()
                .is_none_or(std::collections::HashMap::is_empty)
            && container.hostname.as_deref().is_none_or(str::is_empty)
            && container.dir.as_deref().is_none_or(str::is_empty)
            && !container.tty.unwrap_or(false)
            && !container.open_stdin.unwrap_or(false)
            && !container.read_only.unwrap_or(false)
            && container.stop_signal.as_deref().is_none_or(str::is_empty)
            && container
                .stop_grace_period
                .is_none_or(|value| value == 10_000_000_000)
            && container.hosts.as_ref().is_none_or(Vec::is_empty)
            && container.dns_config.as_ref().is_none_or(|dns| {
                dns.nameservers.as_ref().is_none_or(Vec::is_empty)
                    && dns.search.as_ref().is_none_or(Vec::is_empty)
                    && dns.options.as_ref().is_none_or(Vec::is_empty)
            })
            && container.secrets.as_ref().is_none_or(Vec::is_empty)
            && container.configs.as_ref().is_none_or(Vec::is_empty)
            && container.oom_score_adj.is_none_or(|value| value == 0)
            && container.isolation.is_none_or(|value| {
                matches!(
                    value,
                    bollard::models::TaskSpecContainerSpecIsolationEnum::EMPTY
                        | bollard::models::TaskSpecContainerSpecIsolationEnum::DEFAULT
                )
            })
            && !container.init.unwrap_or(false)
            && container
                .sysctls
                .as_ref()
                .is_none_or(std::collections::HashMap::is_empty)
            && container.capability_add.as_ref().is_none_or(Vec::is_empty)
            && container.capability_drop.as_ref().is_none_or(Vec::is_empty)
            && container.ulimits.as_ref().is_none_or(Vec::is_empty)
    }

    fn no_unsupported_service_settings(spec: &ServiceSpec) -> bool {
        spec.rollback_config.as_ref().is_none_or(|rollback| {
            rollback.parallelism == Some(1)
                && rollback.delay.is_none_or(|value| value == 0)
                && rollback.failure_action
                    == Some(bollard::models::ServiceSpecRollbackConfigFailureActionEnum::PAUSE)
                && rollback.monitor == Some(5 * super::NANOSECONDS_PER_SECOND)
                && rollback.max_failure_ratio == Some(0.0)
                && rollback.order
                    == Some(bollard::models::ServiceSpecRollbackConfigOrderEnum::STOP_FIRST)
        }) && spec.networks.as_ref().is_none_or(Vec::is_empty)
            && spec.endpoint_spec.as_ref().is_none_or(|endpoint| {
                endpoint.mode.is_none_or(|mode| {
                    matches!(
                        mode,
                        bollard::models::EndpointSpecModeEnum::EMPTY
                            | bollard::models::EndpointSpecModeEnum::VIP
                    )
                }) && endpoint.ports.as_ref().is_none_or(Vec::is_empty)
            })
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
                    && mount.volume_options.is_none()
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
                    && network
                        .driver_opts
                        .as_ref()
                        .is_none_or(std::collections::HashMap::is_empty)
            })
        })
    }

    fn health(container: &TaskSpecContainerSpec) -> bool {
        container
            .health_check
            .as_ref()
            .is_none_or(Self::supported_health_config)
    }

    fn resource_limits(task: &TaskSpec) -> bool {
        task.resources
            .as_ref()
            .and_then(|resources| resources.limits.as_ref())
            .is_none_or(|limits| {
                limits
                    .nano_cpus
                    .is_none_or(|value| value >= 0 && value % NANO_CPUS_PER_MILLICORE == 0)
                    && limits.memory_bytes.is_none_or(|value| value >= 0)
                    && limits.pids.is_none_or(|value| value == 0)
            })
            && task.resources.as_ref().is_none_or(|resources| {
                resources
                    .reservations
                    .as_ref()
                    .is_none_or(Self::default_reservations)
            })
    }

    fn default_reservations(reservations: &bollard::models::ResourceObject) -> bool {
        reservations.nano_cpus.is_none_or(|value| value == 0)
            && reservations.memory_bytes.is_none_or(|value| value == 0)
            && reservations
                .generic_resources
                .as_ref()
                .is_none_or(Vec::is_empty)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::DockerError;
    use bollard::models::ServiceSpecRollbackConfig;
    use piqueld_core::manifest::ResourceLimits;
    use piqueld_core::resource::{DesiredService, ResolvedSource};
    use std::collections::BTreeMap;

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

    #[test]
    fn policy_verifies_exactly_the_authored_fields() {
        let image = format!("ghcr.io/example/notes@sha256:{}", "a".repeat(64));
        let desired = DesiredService {
            logical_name: "web".into(),
            name: "app-policy-web".into(),
            source: ResolvedSource::Image {
                requested: "ghcr.io/example/notes:1.4.0".into(),
                digest_reference: image.clone(),
            },
            image,
            replicas: 2,
            environment: BTreeMap::from([("NOTES_PORT".into(), "8080".into())]),
            command: vec!["/bin/notes".into()],
            arguments: vec!["--listen".into(), "8080".into()],
            mounts: Vec::new(),
            healthcheck: None,
            resources: Some(ResourceLimits {
                cpu_millis: Some(250),
                memory_bytes: Some(1024),
            }),
            networks: Vec::new(),
            labels: BTreeMap::new(),
        };
        let authored = BollardDocker::service_spec(&desired).expect("authored specification");
        assert!(ServiceRuntimePolicy::matches(&authored));

        // Known Engine defaults are accepted, while unsupported non-default
        // behavior remains visible as drift.
        let mut echoed = authored.clone();
        let task = echoed.task_template.as_mut().expect("task");
        task.resources.as_mut().expect("resources").reservations = Some(Default::default());
        task.runtime = Some("container".into());
        task.force_update = Some(0);
        task.placement = Some(bollard::models::TaskSpecPlacement::default());
        task.restart_policy.as_mut().expect("restart").max_attempts = Some(0);
        task.container_spec.as_mut().expect("container").dns_config =
            Some(bollard::models::TaskSpecContainerSpecDnsConfig::default());
        echoed.rollback_config = Some(ServiceSpecRollbackConfig {
            parallelism: Some(1),
            failure_action: Some(
                bollard::models::ServiceSpecRollbackConfigFailureActionEnum::PAUSE,
            ),
            monitor: Some(5 * super::super::NANOSECONDS_PER_SECOND),
            max_failure_ratio: Some(0.0),
            order: Some(bollard::models::ServiceSpecRollbackConfigOrderEnum::STOP_FIRST),
            ..Default::default()
        });
        assert!(ServiceRuntimePolicy::matches(&echoed));

        echoed.rollback_config = Some(ServiceSpecRollbackConfig::default());
        if let Some(container) = echoed
            .task_template
            .as_mut()
            .and_then(|task| task.container_spec.as_mut())
        {
            container.stop_grace_period = Some(12_345);
            container.hostname = Some("echoed".into());
        }
        assert!(!ServiceRuntimePolicy::matches(&echoed));

        // Drift in a field piqueld authors is still rejected.
        let mut drifted = authored.clone();
        if let Some(update) = drifted.update_config.as_mut() {
            update.parallelism = Some(4);
        }
        assert!(!ServiceRuntimePolicy::matches(&drifted));

        // Out-of-band security settings are rejected even though piqueld
        // cannot author them.
        let mut injected_capability = authored.clone();
        if let Some(container) = injected_capability
            .task_template
            .as_mut()
            .and_then(|task| task.container_spec.as_mut())
        {
            container.capability_add = Some(vec!["NET_ADMIN".into()]);
        }
        assert!(!ServiceRuntimePolicy::matches(&injected_capability));

        let mut injected_user = authored;
        if let Some(container) = injected_user
            .task_template
            .as_mut()
            .and_then(|task| task.container_spec.as_mut())
        {
            container.user = Some("0".into());
        }
        assert!(!ServiceRuntimePolicy::matches(&injected_user));
    }
}
