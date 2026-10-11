//! One-shot jobs run as Swarm replicated jobs so they share service secrets,
//! networks, mounts, and node placement.
use super::{
    BTreeMap, BollardDocker, DockerError, Duration, HashMap, HealthConfig, InspectServiceOptions,
    JobOutput, JobRuns, JobStatus, ListServicesOptionsBuilder, ListTasksOptionsBuilder,
    ResourceKind, ServiceSpec, ServiceSpecMode, StreamExt, TaskSpecRestartPolicy,
    TaskSpecRestartPolicyConditionEnum,
};
use bollard::{
    container::LogOutput,
    models::{Service, ServiceSpecModeReplicatedJob, Task, TaskState, TaskStatus},
    query_parameters::{ListContainersOptionsBuilder, LogsOptionsBuilder},
};
use piqueld_core::{
    DesiredJobRun,
    api::LogStream,
    resource::{APPLICATION_LABEL, INSTANCE_LABEL, JOB_LABEL, JOB_OPERATION_LABEL, MANAGED_LABEL},
};
use std::time::{SystemTime, UNIX_EPOCH};

/// Output retained from one job run; build history applies its own cap too.
const JOB_OUTPUT_MAX_BYTES: usize = 1024 * 1024;
/// Delay between checks that a removed run's container has stopped.
const JOB_STOP_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Label carrying when a run's timeout expires, in Unix milliseconds.
const JOB_DEADLINE_LABEL: &str = "io.piqueld.job-deadline";

impl BollardDocker {
    /// Converts a service specification into a single-completion job without
    /// restarts or rolling updates. The health check is disabled because an
    /// image `HEALTHCHECK` would otherwise apply, and network aliases are
    /// dropped so the job never answers for the service it was derived from.
    /// The container carries the service's labels, so a run whose service is
    /// already removed can still be found until its container stops.
    pub(super) fn job_spec(mut spec: ServiceSpec) -> ServiceSpec {
        spec.mode = Some(ServiceSpecMode {
            replicated_job: Some(ServiceSpecModeReplicatedJob {
                max_concurrent: Some(1),
                total_completions: Some(1),
            }),
            ..Default::default()
        });
        spec.update_config = None;
        let task = spec.task_template.get_or_insert_with(Default::default);
        task.restart_policy = Some(TaskSpecRestartPolicy {
            condition: Some(TaskSpecRestartPolicyConditionEnum::NONE),
            ..Default::default()
        });
        let container = task.container_spec.get_or_insert_with(Default::default);
        container.health_check = Some(HealthConfig {
            test: Some(vec!["NONE".into()]),
            ..Default::default()
        });
        container.labels.clone_from(&spec.labels);
        for network in task.networks.iter_mut().flatten() {
            network.aliases = None;
        }
        spec
    }

    /// Classifies a run's task status; tasks that have not stopped are running
    /// until `deadline`.
    fn job_task_status(status: TaskStatus, deadline: Option<SystemTime>) -> JobStatus {
        let exit_code = match status.state {
            Some(TaskState::COMPLETE) => {
                return JobStatus::Finished {
                    exit_code: Some(0),
                    error: None,
                };
            }
            Some(TaskState::FAILED) => status
                .container_status
                .as_ref()
                .and_then(|container| container.exit_code),
            Some(
                TaskState::REJECTED | TaskState::SHUTDOWN | TaskState::ORPHANED | TaskState::REMOVE,
            ) => None,
            _ => return JobStatus::Running { deadline },
        };
        JobStatus::Finished {
            exit_code,
            error: status.err.or(status.message),
        }
    }

    /// Replaces any existing service of the job with this operation's run,
    /// labelled with when its timeout expires. An exclusive run starts only
    /// once no other service mounts its volumes and their containers stopped.
    /// The controller's mutation lock covers this and every service write, so
    /// with [`Self::check_volumes_free`] a service never shares a volume with
    /// an exclusive run.
    pub(super) async fn create_job(&self, job: &DesiredJobRun) -> Result<(), DockerError> {
        if !job.has_valid_identity() {
            return Err(DockerError::OwnershipConflict);
        }
        let name = job.container.name.as_str();
        let ownership = &job.container.labels;
        self.remove_owned_service(name, ownership, ResourceKind::Job)
            .await?;
        self.wait_jobs_stopped(ownership, Some(job.logical_name.as_str()), JobRuns::All)
            .await?;
        self.check_volumes_unused(job).await?;
        self.wait_volumes_released(job).await?;
        let node_id = self.local_node_id().await?;
        let mut spec = Self::job_spec(self.runtime_service_spec(&job.container, &node_id).await?);
        let deadline = SystemTime::now() + Duration::from_secs(u64::from(job.timeout_seconds));
        let deadline = deadline
            .duration_since(UNIX_EPOCH)
            .map_err(|_| DockerError::Validation("job deadline"))?
            .as_millis();
        spec.labels
            .get_or_insert_with(Default::default)
            .insert(JOB_DEADLINE_LABEL.into(), deadline.to_string());
        self.create_service_wire(&spec).await
    }

    /// Fails while a service of the application, other than a job, mounts a
    /// volume `job` holds exclusively: waiting cannot remove it.
    async fn check_volumes_unused(&self, job: &DesiredJobRun) -> Result<(), DockerError> {
        if job.exclusive_volumes().next().is_none() {
            return Ok(());
        }
        let services = self
            .labelled_services(Self::ownership_filter(&job.container.labels)?)
            .await?;
        if services.iter().any(|service| {
            let labels = service.spec.as_ref().and_then(|spec| spec.labels.as_ref());
            !labels.is_some_and(|labels| labels.contains_key(JOB_LABEL))
                && Self::spec_mounts(service).any(|(volume, _)| job.holds_volume(volume))
        }) {
            return Err(DockerError::Request(
                "start an exclusive job while a service mounts its volume",
            ));
        }
        Ok(())
    }

    /// Fails while a job of the service's application holds a volume the
    /// service mounts: a job service mounts it writable, a task of a removed
    /// run still may, or a run's container still does. Swarm can report a
    /// task failed, or reap it, before its container exits. Exclusive runs
    /// are the only jobs with writable mounts.
    pub(super) async fn check_volumes_free(
        &self,
        service: &piqueld_core::DesiredService,
    ) -> Result<(), DockerError> {
        if service.mounts.is_empty() {
            return Ok(());
        }
        let mounts = |volume: &str| {
            service
                .mounts
                .iter()
                .any(|mount| mount.volume_name.as_str() == volume)
        };
        let mut labels = Self::ownership_filter(&service.labels)?;
        labels.push(JOB_LABEL.to_owned());
        let jobs = self.labelled_services(labels.clone()).await?;
        let containers = Self::map_request(
            "list job containers",
            self.docker
                .list_containers(Some(
                    ListContainersOptionsBuilder::default()
                        .filters(&HashMap::from([("label", labels)]))
                        .build(),
                ))
                .await,
        )?;
        let application = service.labels.get(APPLICATION_LABEL);
        let held = jobs.iter().any(|job| {
            Self::spec_mounts(job).any(|(volume, read_only)| !read_only && mounts(volume))
        }) || containers
            .iter()
            .flat_map(|container| container.mounts.iter().flatten())
            .any(|mount| mount.rw == Some(true) && mount.name.as_deref().is_some_and(mounts))
            || self
                .volume_tasks(|labels, volume, read_only| {
                    labels.contains_key(JOB_LABEL)
                        && labels.get(APPLICATION_LABEL) == application
                        && !read_only
                        && mounts(volume)
                })
                .await?;
        if held {
            return Err(DockerError::Request(
                "start a service while a job holds its volume",
            ));
        }
        Ok(())
    }

    /// Whether a task that has not stopped mounts a volume `selects` accepts,
    /// given its container labels, the volume name, and whether the mount is
    /// read-only. Tasks of removed services keep their specification until
    /// Swarm stops them, and a task may not have created its container yet,
    /// so tasks rather than containers tell whether a volume is in use.
    async fn volume_tasks(
        &self,
        selects: impl Fn(&HashMap<String, String>, &str, bool) -> bool,
    ) -> Result<bool, DockerError> {
        let tasks = Self::map_request(
            "list tasks",
            self.docker
                .list_tasks(None::<bollard::query_parameters::ListTasksOptions>)
                .await,
        )?;
        let no_labels = HashMap::new();
        Ok(tasks.iter().any(|task| {
            let stopped = matches!(
                task.status.as_ref().and_then(|status| status.state),
                Some(
                    TaskState::COMPLETE
                        | TaskState::SHUTDOWN
                        | TaskState::FAILED
                        | TaskState::REJECTED
                        | TaskState::ORPHANED
                )
            );
            let container = task
                .spec
                .as_ref()
                .and_then(|spec| spec.container_spec.as_ref());
            let labels = container
                .and_then(|container| container.labels.as_ref())
                .unwrap_or(&no_labels);
            !stopped
                && container
                    .and_then(|container| container.mounts.as_ref())
                    .into_iter()
                    .flatten()
                    .filter_map(|mount| {
                        Some((mount.source.as_deref()?, mount.read_only == Some(true)))
                    })
                    .any(|(volume, read_only)| selects(labels, volume, read_only))
        }))
    }

    /// Volume names and read-only flags of a service's specification.
    fn spec_mounts(service: &Service) -> impl Iterator<Item = (&str, bool)> {
        service
            .spec
            .iter()
            .flat_map(|spec| spec.task_template.iter())
            .flat_map(|task| task.container_spec.iter())
            .flat_map(|container| container.mounts.iter().flatten())
            .filter_map(|mount| Some((mount.source.as_deref()?, mount.read_only == Some(true))))
    }

    /// Services carrying every label in `labels`.
    async fn labelled_services(&self, labels: Vec<String>) -> Result<Vec<Service>, DockerError> {
        Self::map_request(
            "list services",
            self.docker
                .list_services(Some(
                    ListServicesOptionsBuilder::default()
                        .filters(&HashMap::from([("label", labels)]))
                        .build(),
                ))
                .await,
        )
    }

    /// Label filters selecting the application named by `ownership`.
    fn ownership_filter(ownership: &BTreeMap<String, String>) -> Result<Vec<String>, DockerError> {
        [MANAGED_LABEL, INSTANCE_LABEL, APPLICATION_LABEL]
            .into_iter()
            .map(|key| {
                ownership
                    .get(key)
                    .map(|value| format!("{key}={value}"))
                    .ok_or(DockerError::Validation("job ownership"))
            })
            .collect()
    }

    /// Waits until no task that has not stopped, and no running container,
    /// mounts a volume `job` holds exclusively. Swarm stops the tasks of
    /// removed services asynchronously; the caller's request timeout bounds
    /// the wait.
    async fn wait_volumes_released(&self, job: &DesiredJobRun) -> Result<(), DockerError> {
        for volume in job.exclusive_volumes() {
            let options = ListContainersOptionsBuilder::default()
                .filters(&HashMap::from([("volume", vec![volume.to_string()])]))
                .build();
            while self
                .volume_tasks(|_, mounted, _| mounted == volume.as_str())
                .await?
                || !Self::map_request(
                    "list volume containers",
                    self.docker.list_containers(Some(options.clone())).await,
                )?
                .is_empty()
            {
                tokio::time::sleep(JOB_STOP_POLL_INTERVAL).await;
            }
        }
        Ok(())
    }

    /// Waits until no selected container of the application's jobs, or of
    /// one job, is running. Swarm stops a removed service's containers
    /// asynchronously, after their grace period; the caller's request timeout
    /// bounds the wait.
    async fn wait_jobs_stopped(
        &self,
        ownership: &BTreeMap<String, String>,
        job: Option<&str>,
        runs: JobRuns<'_>,
    ) -> Result<(), DockerError> {
        let mut labels = Self::ownership_filter(ownership)?;
        labels.push(job.map_or_else(|| JOB_LABEL.to_owned(), |job| format!("{JOB_LABEL}={job}")));
        let options = ListContainersOptionsBuilder::default()
            .filters(&HashMap::from([("label", labels)]))
            .build();
        loop {
            let containers = Self::map_request(
                "list job containers",
                self.docker.list_containers(Some(options.clone())).await,
            )?;
            if !containers.into_iter().any(|container| {
                runs.selects(
                    container
                        .labels
                        .as_ref()
                        .and_then(|labels| labels.get(JOB_OPERATION_LABEL))
                        .map(String::as_str),
                )
            }) {
                return Ok(());
            }
            tokio::time::sleep(JOB_STOP_POLL_INTERVAL).await;
        }
    }

    /// Returns the deadline and tasks of the operation's run, or `None` when it
    /// has no run.
    async fn job_run_tasks(
        &self,
        job: &DesiredJobRun,
    ) -> Result<Option<(Option<SystemTime>, Vec<Task>)>, DockerError> {
        let name = job.container.name.as_str();
        let service = match self
            .docker
            .inspect_service(name, None::<InspectServiceOptions>)
            .await
        {
            Ok(service) => service,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => return Ok(None),
            Err(error) => return Err(DockerError::request("inspect job", error)),
        };
        let labels = service.spec.and_then(|s| s.labels).unwrap_or_default();
        if labels.get(JOB_OPERATION_LABEL).map(String::as_str) != Some(job.operation()) {
            return Ok(None);
        }
        let deadline = labels
            .get(JOB_DEADLINE_LABEL)
            .and_then(|deadline| deadline.parse().ok())
            .map(|deadline| UNIX_EPOCH + Duration::from_millis(deadline));
        if !Self::owns_resource(labels, &job.container.labels, ResourceKind::Job, name) {
            return Err(DockerError::OwnershipConflict);
        }
        let id = service
            .id
            .ok_or(DockerError::Request("read job service identity"))?;
        Self::map_request(
            "list job tasks",
            self.docker
                .list_tasks(Some(
                    ListTasksOptionsBuilder::default()
                        .filters(&HashMap::from([("service", vec![id])]))
                        .build(),
                ))
                .await,
        )
        .map(|tasks| Some((deadline, tasks)))
    }

    /// Reads the progress of the operation's run of the job.
    pub(super) async fn inspect_job(&self, job: &DesiredJobRun) -> Result<JobStatus, DockerError> {
        // A single-completion job without restarts schedules exactly one task.
        Ok(match self.job_run_tasks(job).await? {
            None => JobStatus::Missing,
            Some((deadline, tasks)) => tasks
                .into_iter()
                .find_map(|task| task.status)
                .map_or(JobStatus::Running { deadline }, |status| {
                    Self::job_task_status(status, deadline)
                }),
        })
    }

    /// Reads the run's output up to [`JOB_OUTPUT_MAX_BYTES`], merging
    /// consecutive chunks of one stream so build history stores them in few
    /// writes.
    pub(super) async fn read_job_output(
        &self,
        job: &DesiredJobRun,
    ) -> Result<JobOutput, DockerError> {
        let container = self
            .job_run_tasks(job)
            .await?
            .into_iter()
            .flat_map(|(_, tasks)| tasks)
            .find_map(|task| task.status?.container_status?.container_id);
        let Some(container) = container else {
            return Ok(JobOutput::default());
        };
        let mut logs = self.docker.logs(
            &container,
            Some(
                LogsOptionsBuilder::default()
                    .stdout(true)
                    .stderr(true)
                    .build(),
            ),
        );
        let mut output = JobOutput::default();
        let mut remaining = JOB_OUTPUT_MAX_BYTES;
        while let Some(item) = logs.next().await {
            let (stream, message) = match item {
                Ok(LogOutput::StdErr { message }) => (LogStream::Stderr, message),
                Ok(
                    LogOutput::StdOut { message }
                    | LogOutput::Console { message }
                    | LogOutput::StdIn { message },
                ) => (LogStream::Stdout, message),
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => break,
                Err(error) => return Err(DockerError::request("read job output", error)),
            };
            let kept = &message[..message.len().min(remaining)];
            remaining -= kept.len();
            match output.chunks.last_mut() {
                Some((last, bytes)) if *last == stream => bytes.extend_from_slice(kept),
                _ if kept.is_empty() => {}
                _ => output.chunks.push((stream, kept.to_vec())),
            }
            if kept.len() < message.len() {
                output.truncated = true;
                break;
            }
        }
        Ok(output)
    }

    /// Deletes the selected job services of the application named by
    /// `ownership`, then waits for their containers to stop, including those
    /// of services an earlier attempt already deleted.
    pub(super) async fn remove_owned_jobs(
        &self,
        ownership: &BTreeMap<String, String>,
        runs: JobRuns<'_>,
    ) -> Result<(), DockerError> {
        let application = ownership
            .get(APPLICATION_LABEL)
            .ok_or(DockerError::Validation("job ownership"))?;
        let services = Self::map_request(
            "list jobs",
            self.docker
                .list_services(Some(
                    ListServicesOptionsBuilder::default()
                        .filters(&HashMap::from([(
                            "label",
                            vec![
                                format!("{APPLICATION_LABEL}={application}"),
                                JOB_LABEL.to_owned(),
                            ],
                        )]))
                        .build(),
                ))
                .await,
        )?;
        for spec in services.into_iter().filter_map(|service| service.spec) {
            let operation = spec
                .labels
                .as_ref()
                .and_then(|labels| labels.get(JOB_OPERATION_LABEL))
                .map(String::as_str);
            let Some(name) = spec.name.filter(|_| runs.selects(operation)) else {
                continue;
            };
            match self
                .remove_owned_service(&name, ownership, ResourceKind::Job)
                .await
            {
                // Same-label services of another instance are not ours to remove.
                Ok(()) | Err(DockerError::OwnershipConflict) => {}
                Err(error) => return Err(error),
            }
        }
        self.wait_jobs_stopped(ownership, None, runs).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::models::{
        ContainerStatus, NetworkAttachmentConfig, TaskSpec, TaskSpecContainerSpec,
    };

    #[test]
    fn job_spec_runs_once_without_health_check_or_service_alias() {
        let labels = HashMap::from([(JOB_OPERATION_LABEL.to_owned(), "operation-1".to_owned())]);
        let service = ServiceSpec {
            labels: Some(labels.clone()),
            task_template: Some(TaskSpec {
                container_spec: Some(TaskSpecContainerSpec {
                    health_check: None,
                    ..Default::default()
                }),
                networks: Some(vec![NetworkAttachmentConfig {
                    target: Some("app-notes".into()),
                    aliases: Some(vec!["web".into()]),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let spec = BollardDocker::job_spec(service);
        let mode = spec.mode.unwrap();
        assert!(mode.replicated.is_none());
        let job = mode.replicated_job.unwrap();
        assert_eq!(
            (job.max_concurrent, job.total_completions),
            (Some(1), Some(1))
        );
        assert!(spec.update_config.is_none());
        let task = spec.task_template.unwrap();
        assert_eq!(
            task.restart_policy.unwrap().condition,
            Some(TaskSpecRestartPolicyConditionEnum::NONE)
        );
        let container = task.container_spec.unwrap();
        assert_eq!(
            container.health_check.unwrap().test,
            Some(vec!["NONE".into()])
        );
        // Labels find a run's container after its service is removed.
        assert_eq!(container.labels, Some(labels));
        let network = &task.networks.unwrap()[0];
        assert_eq!(network.target.as_deref(), Some("app-notes"));
        assert_eq!(network.aliases, None);
    }

    #[test]
    fn task_status_keeps_exit_code_and_docker_explanation() {
        let status = |state, exit_code, err: Option<&str>| TaskStatus {
            state: Some(state),
            err: err.map(Into::into),
            container_status: Some(ContainerStatus {
                exit_code,
                ..Default::default()
            }),
            ..Default::default()
        };
        let deadline = Some(std::time::UNIX_EPOCH);
        assert_eq!(
            BollardDocker::job_task_status(status(TaskState::RUNNING, None, None), deadline),
            JobStatus::Running { deadline }
        );
        assert_eq!(
            BollardDocker::job_task_status(status(TaskState::COMPLETE, Some(0), None), deadline),
            JobStatus::Finished {
                exit_code: Some(0),
                error: None
            }
        );
        assert_eq!(
            BollardDocker::job_task_status(
                status(TaskState::FAILED, Some(3), Some("task: non-zero exit (3)")),
                deadline
            ),
            JobStatus::Finished {
                exit_code: Some(3),
                error: Some("task: non-zero exit (3)".into())
            }
        );
        assert_eq!(
            BollardDocker::job_task_status(
                status(TaskState::REJECTED, None, Some("executable file not found")),
                deadline
            ),
            JobStatus::Finished {
                exit_code: None,
                error: Some("executable file not found".into())
            }
        );
    }
}
