use super::policy::ServiceRuntimePolicy;
use super::{
    BTreeMap, BollardDocker, Convergence, DesiredMount, DockerError, HealthCheck, HealthConfig,
    InspectContainerOptionsBuilder, MountTypeEnum, NANO_CPUS_PER_MILLICORE, NANOSECONDS_PER_SECOND,
    ObservedService, ObservedTask, ResourceLimits, ServiceSpec, TaskDiagnostic,
    TaskSpecContainerSpec, TaskState,
};
use bollard::models::HealthStatusEnum;

impl BollardDocker {
    /// Converts one Docker task into the backend-neutral observation contract.
    ///
    /// `healthy` carries the container healthcheck verdict for running tasks;
    /// it is `None` when the task has no healthcheck, has not converged to a
    /// verdict yet, or its container could not be inspected.
    pub(super) fn observe_task(
        task: &bollard::models::Task,
        healthy: Option<bool>,
    ) -> ObservedTask {
        let state = Self::task_state(task);
        let diagnostic = match state {
            TaskState::Failed => Some(TaskDiagnostic::Failed {
                exit_code: task
                    .status
                    .as_ref()
                    .and_then(|status| status.container_status.as_ref())
                    .and_then(|status| status.exit_code),
            }),
            TaskState::Rejected => Some(TaskDiagnostic::Rejected),
            _ => None,
        };
        ObservedTask {
            state,
            healthy,
            desired_running: task.desired_state == Some(bollard::models::TaskState::RUNNING),
            diagnostic,
        }
    }

    /// Reads the live healthcheck verdict of one container.
    ///
    /// A vanished container reports no verdict instead of failing the whole
    /// observation; tasks are transient and disappear while services update.
    pub(super) async fn container_health(
        &self,
        container_id: &str,
    ) -> Result<Option<bool>, DockerError> {
        match self
            .docker
            .inspect_container(
                container_id,
                Some(InspectContainerOptionsBuilder::default().build()),
            )
            .await
        {
            Ok(inspection) => Ok(Self::health_verdict(
                inspection.state.and_then(|state| state.health),
            )),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(None),
            Err(error) => Err(DockerError::request("inspect container health", error)),
        }
    }

    /// Converts Docker's reported healthcheck result into the neutral verdict.
    fn health_verdict(health: Option<bollard::models::Health>) -> Option<bool> {
        match health.and_then(|health| health.status) {
            Some(HealthStatusEnum::HEALTHY) => Some(true),
            Some(HealthStatusEnum::UNHEALTHY) => Some(false),
            // "none" (no healthcheck), "starting", and absent states carry no
            // verdict yet and must not fail an otherwise running task.
            Some(HealthStatusEnum::NONE | HealthStatusEnum::STARTING | HealthStatusEnum::EMPTY)
            | None => None,
        }
    }

    /// Maps Bollard's task state enum to the backend-neutral task state.
    fn task_state(task: &bollard::models::Task) -> TaskState {
        match task
            .status
            .as_ref()
            .and_then(|status| status.state)
            .unwrap_or_default()
        {
            bollard::models::TaskState::NEW => TaskState::New,
            bollard::models::TaskState::PENDING => TaskState::Pending,
            bollard::models::TaskState::ASSIGNED => TaskState::Assigned,
            bollard::models::TaskState::ACCEPTED => TaskState::Accepted,
            bollard::models::TaskState::PREPARING => TaskState::Preparing,
            bollard::models::TaskState::STARTING => TaskState::Starting,
            bollard::models::TaskState::RUNNING => TaskState::Running,
            bollard::models::TaskState::COMPLETE => TaskState::Complete,
            bollard::models::TaskState::FAILED => TaskState::Failed,
            bollard::models::TaskState::REJECTED => TaskState::Rejected,
            bollard::models::TaskState::SHUTDOWN => TaskState::Shutdown,
            _ => TaskState::Unknown,
        }
    }

    /// Converts one Docker service specification and its tasks into an observation.
    pub(super) fn observe_service(
        spec: &ServiceSpec,
        tasks: Vec<ObservedTask>,
        update: Option<bollard::models::ServiceUpdateStatusStateEnum>,
    ) -> Result<ObservedService, DockerError> {
        let container = spec
            .task_template
            .as_ref()
            .and_then(|t| t.container_spec.as_ref())
            .ok_or(DockerError::Request("read service container specification"))?;
        let name = spec
            .name
            .clone()
            .ok_or(DockerError::Request("read service name"))?;
        let replicas = BollardDocker::replica_count(spec);
        let labels = spec
            .labels
            .clone()
            .unwrap_or_default()
            .into_iter()
            .collect();
        let runtime_configuration_matches = ServiceRuntimePolicy::matches(spec);
        let healthcheck_configured = container.health_check.is_some();
        let convergence =
            BollardDocker::convergence(&tasks, replicas, update, healthcheck_configured);
        Ok(ObservedService {
            name,
            image: container.image.clone().unwrap_or_default(),
            replicas,
            environment: BollardDocker::observed_environment(container),
            command: container.command.clone().unwrap_or_default(),
            arguments: container.args.clone().unwrap_or_default(),
            mounts: BollardDocker::observed_mounts(container),
            healthcheck: container
                .health_check
                .as_ref()
                .and_then(BollardDocker::observed_health),
            resources: BollardDocker::observed_resources(spec),
            networks: BollardDocker::observed_networks(spec),
            labels,
            runtime_configuration_matches,
            tasks,
            convergence,
        })
    }

    pub(super) fn observed_environment(
        container: &TaskSpecContainerSpec,
    ) -> BTreeMap<String, String> {
        container
            .env
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| {
                entry
                    .split_once('=')
                    .map(|(key, value)| (key.into(), value.into()))
            })
            .collect()
    }

    pub(super) fn observed_mounts(container: &TaskSpecContainerSpec) -> Vec<DesiredMount> {
        container
            .mounts
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter(|mount| mount.typ == Some(MountTypeEnum::VOLUME))
            .filter_map(|mount| {
                Some(DesiredMount {
                    volume_name: mount.source?,
                    target: mount.target?,
                    read_only: mount.read_only.unwrap_or(false),
                })
            })
            .collect()
    }

    pub(super) fn observed_resources(spec: &ServiceSpec) -> Option<ResourceLimits> {
        spec.task_template
            .as_ref()
            .and_then(|task| task.resources.as_ref())
            .and_then(|resources| resources.limits.as_ref())
            .map(|limits| ResourceLimits {
                cpu_millis: limits
                    .nano_cpus
                    .and_then(|nano_cpus| u32::try_from(nano_cpus / NANO_CPUS_PER_MILLICORE).ok()),
                memory_bytes: limits
                    .memory_bytes
                    .and_then(|bytes| u64::try_from(bytes).ok()),
            })
    }

    pub(super) fn observed_networks(spec: &ServiceSpec) -> Vec<String> {
        spec.task_template
            .as_ref()
            .and_then(|task| task.networks.clone())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|network| network.target)
            .collect()
    }

    pub(super) fn replica_count(spec: &ServiceSpec) -> u16 {
        spec.mode
            .as_ref()
            .and_then(|mode| mode.replicated.as_ref())
            .and_then(|replicated| replicated.replicas)
            .and_then(|replicas| u16::try_from(replicas).ok())
            .unwrap_or(0)
    }

    pub(super) fn convergence(
        tasks: &[ObservedTask],
        replicas: u16,
        update: Option<bollard::models::ServiceUpdateStatusStateEnum>,
        healthcheck_configured: bool,
    ) -> Convergence {
        if matches!(
            update,
            Some(
                bollard::models::ServiceUpdateStatusStateEnum::PAUSED
                    | bollard::models::ServiceUpdateStatusStateEnum::ROLLBACK_PAUSED
            )
        ) {
            return Convergence::Failed;
        }
        if matches!(
            update,
            Some(
                bollard::models::ServiceUpdateStatusStateEnum::UPDATING
                    | bollard::models::ServiceUpdateStatusStateEnum::ROLLBACK_STARTED
            )
        ) {
            return Convergence::Updating;
        }
        let running = tasks
            .iter()
            .filter(|task| {
                task.desired_running
                    && task.state == TaskState::Running
                    && if healthcheck_configured {
                        task.healthy == Some(true)
                    } else {
                        task.healthy != Some(false)
                    }
            })
            .count();
        let failed = tasks
            .iter()
            .filter(|task| {
                task.desired_running
                    && (matches!(task.state, TaskState::Failed | TaskState::Rejected)
                        || task.healthy == Some(false))
            })
            .count();
        match (running, failed) {
            (running, _) if running == usize::from(replicas) => Convergence::Converged,
            (0, failed) if failed > 0 => Convergence::Failed,
            (_, failed) if failed > 0 => Convergence::Degraded,
            _ => Convergence::Updating,
        }
    }

    /// Converts Docker's supported health-check syntax into the core model.
    pub(super) fn observed_health(config: &HealthConfig) -> Option<HealthCheck> {
        let test = config.test.as_ref()?;
        let interval = u32::try_from(config.interval? / NANOSECONDS_PER_SECOND).ok()?;
        let timeout = u32::try_from(config.timeout? / NANOSECONDS_PER_SECOND).ok()?;
        match test.first().map(String::as_str) {
            Some("CMD") => Some(
                Self::observed_wget_health(test, interval, timeout).unwrap_or(
                    HealthCheck::Command {
                        command: test[1..].to_vec(),
                        interval_seconds: interval,
                        timeout_seconds: timeout,
                    },
                ),
            ),
            Some("CMD-SHELL") => {
                let shell = test.get(1)?;
                let url = shell.split_whitespace().last()?;
                Self::observed_http_health(url, interval, timeout)
            }
            _ => None,
        }
    }

    /// Recognizes Docker's reserved canonical HTTP `wget` health-check vector.
    pub(super) fn observed_wget_health(
        test: &[String],
        interval_seconds: u32,
        timeout_seconds: u32,
    ) -> Option<HealthCheck> {
        if test.len() != 8 {
            return None;
        }
        let [
            _,
            wget,
            quiet,
            timeout_flag,
            timeout,
            output_flag,
            output_path,
            url,
        ] = test
        else {
            return None;
        };
        let expected_timeout = timeout_seconds.to_string();
        if wget != "wget"
            || quiet != "-q"
            || timeout_flag != "-T"
            || timeout != &expected_timeout
            || output_flag != "-O"
            || output_path != "/dev/null"
        {
            return None;
        }
        Self::observed_http_health(url, interval_seconds, timeout_seconds)
    }

    fn observed_http_health(
        url: &str,
        interval_seconds: u32,
        timeout_seconds: u32,
    ) -> Option<HealthCheck> {
        let url = url.strip_prefix("http://127.0.0.1:")?;
        let (port, path) = url
            .split_once('/')
            .map_or((url, "/"), |(port, _)| (port, &url[port.len()..]));
        Some(HealthCheck::Http {
            port: port.parse().ok()?,
            path: path.into(),
            interval_seconds,
            timeout_seconds,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(healthy: Option<bool>) -> ObservedTask {
        ObservedTask {
            state: TaskState::Running,
            healthy,
            desired_running: true,
            diagnostic: None,
        }
    }

    #[test]
    fn pending_healthcheck_does_not_count_as_converged() {
        assert_eq!(
            BollardDocker::convergence(&[running(None)], 1, None, true),
            Convergence::Updating
        );
        assert_eq!(
            BollardDocker::convergence(&[running(Some(true))], 1, None, true),
            Convergence::Converged
        );
        assert_eq!(
            BollardDocker::convergence(&[running(None)], 1, None, false),
            Convergence::Converged
        );
    }
}
