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
    models::{ServiceSpecModeReplicatedJob, Task, TaskState, TaskStatus},
    query_parameters::{ListContainersOptionsBuilder, LogsOptionsBuilder},
};
use piqueld_core::{
    DesiredJobRun,
    api::LogStream,
    resource::{APPLICATION_LABEL, INSTANCE_LABEL, JOB_LABEL, JOB_OPERATION_LABEL, MANAGED_LABEL},
};

/// Output retained from one job run; build history applies its own cap too.
const JOB_OUTPUT_MAX_BYTES: usize = 1024 * 1024;
/// Delay between checks that a removed run's container has stopped.
const JOB_STOP_POLL_INTERVAL: Duration = Duration::from_millis(250);

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

    /// Classifies a run's task status; tasks that have not stopped are running.
    fn job_task_status(status: TaskStatus) -> JobStatus {
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
            _ => return JobStatus::Running,
        };
        JobStatus::Finished {
            exit_code,
            error: status.err.or(status.message),
        }
    }

    /// Replaces any existing service of the job with this operation's run.
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
        let node_id = self.local_node_id().await?;
        let spec = Self::job_spec(
            self.service_spec_with_secrets(&job.container, &node_id)
                .await?,
        );
        self.create_service_wire(&spec).await
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
        let mut labels = [MANAGED_LABEL, INSTANCE_LABEL, APPLICATION_LABEL]
            .into_iter()
            .map(|key| {
                ownership
                    .get(key)
                    .map(|value| format!("{key}={value}"))
                    .ok_or(DockerError::Validation("job ownership"))
            })
            .collect::<Result<Vec<_>, _>>()?;
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

    /// Returns the tasks of the operation's run, or `None` when it has no run.
    async fn job_run_tasks(&self, job: &DesiredJobRun) -> Result<Option<Vec<Task>>, DockerError> {
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
        .map(Some)
    }

    /// Reads the progress of the operation's run of the job.
    pub(super) async fn inspect_job(&self, job: &DesiredJobRun) -> Result<JobStatus, DockerError> {
        // A single-completion job without restarts schedules exactly one task.
        Ok(match self.job_run_tasks(job).await? {
            None => JobStatus::Missing,
            Some(tasks) => tasks
                .into_iter()
                .find_map(|task| task.status)
                .map_or(JobStatus::Running, Self::job_task_status),
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
            .flatten()
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
        assert_eq!(
            BollardDocker::job_task_status(status(TaskState::RUNNING, None, None)),
            JobStatus::Running
        );
        assert_eq!(
            BollardDocker::job_task_status(status(TaskState::COMPLETE, Some(0), None)),
            JobStatus::Finished {
                exit_code: Some(0),
                error: None
            }
        );
        assert_eq!(
            BollardDocker::job_task_status(status(
                TaskState::FAILED,
                Some(3),
                Some("task: non-zero exit (3)")
            )),
            JobStatus::Finished {
                exit_code: Some(3),
                error: Some("task: non-zero exit (3)".into())
            }
        );
        assert_eq!(
            BollardDocker::job_task_status(status(
                TaskState::REJECTED,
                None,
                Some("executable file not found")
            )),
            JobStatus::Finished {
                exit_code: None,
                error: Some("executable file not found".into())
            }
        );
    }
}
