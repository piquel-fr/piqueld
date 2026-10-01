//! One-shot jobs run as Swarm replicated jobs so they share service secrets,
//! networks, mounts, and node placement.
use super::{
    BTreeMap, BollardDocker, DockerError, HashMap, JobStatus, ListServicesOptionsBuilder,
    ListTasksOptionsBuilder, ResourceKind, ServiceSpecMode, StreamExt, TaskSpecRestartPolicy,
    TaskSpecRestartPolicyConditionEnum,
};
use bollard::{
    container::LogOutput,
    models::{ServiceSpecModeReplicatedJob, TaskState},
    query_parameters::LogsOptionsBuilder,
};
use piqueld_core::{
    DesiredJob,
    api::LogStream,
    resource::{APPLICATION_LABEL, JOB_LABEL},
};

/// Output retained from one job run; build history applies its own cap too.
const JOB_OUTPUT_MAX_BYTES: usize = 1024 * 1024;

impl BollardDocker {
    /// Replaces any earlier run of the job with a single-completion replicated job.
    pub(super) async fn create_job(&self, job: &DesiredJob) -> Result<(), DockerError> {
        if !job.has_valid_identity() {
            return Err(DockerError::OwnershipConflict);
        }
        self.remove_owned_service(
            job.container.name.as_str(),
            &job.container.labels,
            ResourceKind::Job,
        )
        .await?;
        let node_id = self.local_node_id().await?;
        let mut spec = self
            .service_spec_with_secrets(&job.container, &node_id)
            .await?;
        spec.mode = Some(ServiceSpecMode {
            replicated_job: Some(ServiceSpecModeReplicatedJob {
                max_concurrent: Some(1),
                total_completions: Some(1),
            }),
            ..Default::default()
        });
        spec.update_config = None;
        spec.task_template
            .as_mut()
            .expect("service specifications include a task template")
            .restart_policy = Some(TaskSpecRestartPolicy {
            condition: Some(TaskSpecRestartPolicyConditionEnum::NONE),
            ..Default::default()
        });
        self.create_service_wire(&spec).await
    }

    /// Reads the job's task state and, once it stopped, its container output.
    pub(super) async fn inspect_job(&self, job: &DesiredJob) -> Result<JobStatus, DockerError> {
        let tasks = Self::map_request(
            "list job tasks",
            self.docker
                .list_tasks(Some(
                    ListTasksOptionsBuilder::default()
                        .filters(&HashMap::from([(
                            "service",
                            vec![job.container.name.to_string()],
                        )]))
                        .build(),
                ))
                .await,
        )?;
        // A single-completion job without restarts schedules exactly one task.
        let Some(status) = tasks.into_iter().find_map(|task| task.status) else {
            return Ok(JobStatus::Running);
        };
        let exit_code = match status.state {
            Some(TaskState::COMPLETE) => Some(0),
            Some(TaskState::FAILED) => status
                .container_status
                .as_ref()
                .and_then(|container| container.exit_code),
            Some(TaskState::REJECTED | TaskState::SHUTDOWN | TaskState::ORPHANED) => None,
            _ => return Ok(JobStatus::Running),
        };
        let output = match status
            .container_status
            .and_then(|container| container.container_id)
        {
            Some(container) => self.job_output(&container).await?,
            None => Vec::new(),
        };
        Ok(JobStatus::Finished { exit_code, output })
    }

    async fn job_output(&self, container: &str) -> Result<Vec<(LogStream, Vec<u8>)>, DockerError> {
        let mut logs = self.docker.logs(
            container,
            Some(
                LogsOptionsBuilder::default()
                    .stdout(true)
                    .stderr(true)
                    .build(),
            ),
        );
        let mut output = Vec::new();
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
            let kept = message.len().min(remaining);
            output.push((stream, message[..kept].to_vec()));
            remaining -= kept;
            if remaining == 0 {
                break;
            }
        }
        Ok(output)
    }

    /// Deletes every owned job service of the application named by `ownership`.
    pub(super) async fn remove_owned_jobs(
        &self,
        ownership: &BTreeMap<String, String>,
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
        for name in services
            .into_iter()
            .filter_map(|service| service.spec?.name)
        {
            match self
                .remove_owned_service(&name, ownership, ResourceKind::Job)
                .await
            {
                // Same-label services of another instance are not ours to remove.
                Ok(()) | Err(DockerError::OwnershipConflict) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}
