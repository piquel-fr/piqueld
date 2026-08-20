use super::policy::ServiceRuntimePolicy;
use super::{
    BTreeMap, BollardDocker, Convergence, DesiredMount, DockerError, HealthCheck, HealthConfig,
    MountTypeEnum, NANO_CPUS_PER_MILLICORE, NANOSECONDS_PER_SECOND, ObservedService, ObservedTask,
    ResourceLimits, ServiceSpec, TaskDiagnostic, TaskSpecContainerSpec, TaskState,
};

impl BollardDocker {
    /// Converts one Docker task into the backend-neutral observation contract.
    pub(super) fn observe_task(task: &bollard::models::Task) -> ObservedTask {
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
            healthy: None,
            desired_running: task.desired_state == Some(bollard::models::TaskState::RUNNING),
            diagnostic,
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
        let convergence = BollardDocker::convergence(&tasks, replicas, update);
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
                    && task.healthy != Some(false)
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
            Some("CMD") => Some(HealthCheck::Command {
                command: test[1..].to_vec(),
                interval_seconds: interval,
                timeout_seconds: timeout,
            }),
            Some("CMD-SHELL") => {
                let shell = test.get(1)?;
                let url = shell
                    .split_whitespace()
                    .last()?
                    .strip_prefix("http://127.0.0.1:")?;
                let (port, path) = url
                    .split_once('/')
                    .map_or((url, "/"), |(port, _)| (port, &url[port.len()..]));
                Some(HealthCheck::Http {
                    port: port.parse().ok()?,
                    path: path.into(),
                    interval_seconds: interval,
                    timeout_seconds: timeout,
                })
            }
            _ => None,
        }
    }
}
