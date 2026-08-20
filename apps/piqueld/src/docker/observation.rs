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

    /// Maps a Bollard task to the backend-neutral task state.
    ///
    /// Missing or unsupported task states are mapped to [`TaskState::Unknown`].
    ///
    /// # Examples
    ///
    /// ```
    /// let task = bollard::models::Task::default();
    /// assert_eq!(task_state(&task), TaskState::Unknown);
    /// ```
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

    /// Builds a backend-neutral service observation from a Docker service specification and its tasks.
    ///
    /// # Errors
    ///
    /// Returns `DockerError` when the service lacks a container specification or name.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// let spec: &ServiceSpec = todo!();
    /// let tasks: Vec<ObservedTask> = todo!();
    /// let update = None;
    ///
    /// let observed = BollardDocker::observe_service(spec, tasks, update)?;
    /// # Ok::<(), DockerError>(())
    /// ```
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

    /// Converts container environment entries into a key-value map, ignoring entries without an equals sign.
    ///
    /// # Examples
    ///
    /// ```
    /// let container = bollard::models::TaskSpecContainerSpec {
    ///     env: Some(vec!["MODE=production".into(), "DEBUG".into()]),
    ///     ..Default::default()
    /// };
    ///
    /// let environment = BollardDocker::observed_environment(&container);
    /// assert_eq!(environment.get("MODE"), Some(&"production".to_owned()));
    /// assert!(!environment.contains_key("DEBUG"));
    /// ```
    ///
    /// # Returns
    ///
    /// A map of environment variable names to their values.
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

    /// Converts volume mounts with complete source and target values into desired mounts.
    ///
    /// Read-only mounts default to `false`; non-volume or incomplete mounts are omitted.
    ///
    /// # Examples
    ///
    /// ```
    /// let container = TaskSpecContainerSpec {
    ///     mounts: None,
    ///     ..Default::default()
    /// };
    /// let mounts = BollardDocker::observed_mounts(&container);
    /// assert!(mounts.is_empty());
    /// ```
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

    /// Extracts resource limits from a service specification.
    ///
    /// CPU limits are converted from nanocpus to millicores, and nonnegative memory
    /// limits are retained in bytes. Missing task, resource, or limit specifications
    /// produce `None`.
    ///
    /// # Examples
    ///
    /// ```
    /// let spec = ServiceSpec::default();
    /// assert!(BollardDocker::observed_resources(&spec).is_none());
    /// ```
    ///
    /// Returns the observed resource limits, if configured.
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

    /// Extracts the target identifiers from the service's configured networks.
    ///
    /// # Examples
    ///
    /// ```
    /// let spec = ServiceSpec::default();
    /// let networks = BollardDocker::observed_networks(&spec);
    ///
    /// assert!(networks.is_empty());
    /// ```
    ///
    /// # Returns
    ///
    /// The target identifiers of configured networks that specify a target.
    pub(super) fn observed_networks(spec: &ServiceSpec) -> Vec<String> {
        spec.task_template
            .as_ref()
            .and_then(|task| task.networks.clone())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|network| network.target)
            .collect()
    }

    /// Extracts the replicated service's replica count.
    ///
    /// Values outside the `u16` range and services without a replicated mode produce `0`.
    ///
    /// # Examples
    ///
    /// ```
    /// let spec = ServiceSpec::default();
    /// assert_eq!(BollardDocker::replica_count(&spec), 0);
    /// ```
    pub(super) fn replica_count(spec: &ServiceSpec) -> u16 {
        spec.mode
            .as_ref()
            .and_then(|mode| mode.replicated.as_ref())
            .and_then(|replicated| replicated.replicas)
            .and_then(|replicas| u16::try_from(replicas).ok())
            .unwrap_or(0)
    }

    /// Determines whether a service has converged, failed, degraded, or remains updating.
    ///
    /// Paused updates fail immediately. Otherwise, the result is based on the update
    /// status and the number of healthy running tasks compared with the desired
    /// replica count.
    ///
    /// # Examples
    ///
    /// ```
    /// assert!(matches!(
    ///     BollardDocker::convergence(&[], 0, None),
    ///     Convergence::Converged
    /// ));
    /// ```
    pub(super) fn convergence(
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

    /// Converts supported Docker health-check definitions into core health checks.
    ///
    /// `CMD` checks are interpreted as HTTP checks when they match the supported
    /// `wget` form; other `CMD` checks become command checks. `CMD-SHELL` checks
    /// are converted when they end with a supported localhost HTTP URL.
    ///
    /// # Examples
    ///
    /// ```
    /// let config = HealthConfig {
    ///     test: Some(vec![
    ///         "CMD-SHELL".into(),
    ///         "wget -qO- http://127.0.0.1:8080/health".into(),
    ///     ]),
    ///     interval: Some(10_000_000_000),
    ///     timeout: Some(5_000_000_000),
    ///     ..Default::default()
    /// };
    ///
    /// assert!(BollardDocker::observed_health(&config).is_some());
    /// ```
    ///
    /// Returns `None` for unsupported or incomplete health-check definitions.
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

    /// Recognizes a supported `wget` health-check command and converts it to an HTTP health check.
    ///
    /// The command must use the expected quiet mode, timeout, output path, and argument layout.
    /// Unsupported commands or URLs return `None`.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let test = vec![
    ///     "CMD".into(),
    ///     "wget".into(),
    ///     "-q".into(),
    ///     "-T".into(),
    ///     "5".into(),
    ///     "-O".into(),
    ///     "/dev/null".into(),
    ///     "http://127.0.0.1:8080/health".into(),
    /// ];
    ///
    /// assert!(BollardDocker::observed_wget_health(&test, 10, 5).is_some());
    /// ```
    ///
    /// # Arguments
    ///
    /// * `test` - The Docker health-check command arguments.
    /// * `interval_seconds` - The interval between health checks.
    /// * `timeout_seconds` - The health-check timeout.
    fn observed_wget_health(
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

    /// Converts a localhost HTTP URL into an HTTP health check.
    ///
    /// # Examples
    ///
    /// ```
    /// let health = observed_http_health("http://127.0.0.1:8080/health", 10, 2);
    /// assert!(health.is_some());
    /// ```
    fn observed_http_health(
    url: &str,
    interval_seconds: u32,
    timeout_seconds: u32,
    ) -> Option<HealthCheck> {
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
