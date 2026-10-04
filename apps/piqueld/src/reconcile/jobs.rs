//! One-shot jobs run after preparation and before promotion. Their service's
//! startup dependencies converge first; other services wait for job success.
//!
//! Each job succeeds at most once per operation. Retries and restarts skip jobs
//! with a recorded success, and resume a run that is still running or
//! succeeded instead of starting it again. A run's outcome is recorded before
//! its service is removed, so a crash in between never reruns it.
use super::{
    ActionKind, CancellationToken, Controller, DockerApi, Duration, Operation, OperationError,
    Plan, PlanRequest,
};
use crate::{
    build::BuildLog,
    docker::{JobRuns, JobStatus},
};
use piqueld_core::{
    DesiredJobRun, ResolvedApplication,
    api::{BuildState, LogStream},
    manifest::JobRun,
};
use std::{collections::BTreeMap, sync::Arc};

/// Delay between job progress checks.
const JOB_POLL_INTERVAL: Duration = Duration::from_secs(1);

impl<D: DockerApi> Controller<D> {
    /// Removes runs of earlier operations, then runs before-rollout jobs in
    /// declared order. Promoted operations already passed this point, so their
    /// retries and repairs never rerun jobs.
    pub(super) async fn run_jobs(
        &self,
        operation: &Operation,
        target: &ResolvedApplication,
        cancellation: &CancellationToken,
    ) -> Result<(), OperationError> {
        if self.store.is_promoted(&operation.id).await? {
            return Ok(());
        }
        let succeeded = self.store.succeeded_jobs(&operation.id).await?;
        let jobs = target
            .jobs
            .iter()
            .filter(|job| {
                job.run == JobRun::BeforeRollout && !succeeded.contains(job.logical_name.as_str())
            })
            .map(|job| job.for_operation(&operation.id))
            .collect::<Vec<_>>();
        let ownership = self.ownership_labels(&operation.application_id);
        // A run left by an earlier operation must not overlap this deployment,
        // even one without jobs. This operation's own runs are kept only while
        // a job is pending, because one of them may be resumed.
        let stale = if jobs.is_empty() {
            JobRuns::All
        } else {
            JobRuns::Except(&operation.id)
        };
        self.remove_jobs(operation, &ownership, stale, cancellation)
            .await?;
        if jobs.is_empty() {
            return Ok(());
        }
        for job in &jobs {
            self.prepare_job_dependencies(operation, target, job, &ownership, cancellation)
                .await?;
            self.run_job(operation, job, &ownership, cancellation)
                .await?;
        }
        Ok(())
    }

    /// Uses the full rollout plan to check blockers, but executes only shared
    /// infrastructure and this job's startup dependencies. Replanning preserves
    /// dependency ordering without promoting the candidate or touching other
    /// services. Each dependency receives its own convergence budget.
    async fn prepare_job_dependencies(
        &self,
        operation: &Operation,
        target: &ResolvedApplication,
        job: &DesiredJobRun,
        ownership: &BTreeMap<String, String>,
        cancellation: &CancellationToken,
    ) -> Result<(), OperationError> {
        let dependencies = target.job_dependencies(job);
        let mut deadline = tokio::time::Instant::now() + self.retry.convergence_timeout;
        loop {
            let observed = self
                .observe_with_retry(operation, cancellation, deadline)
                .await?;
            let accepted_routes = self.store.applied_routes(&operation.application_id).await?;
            let plan = Plan::from_request(
                &PlanRequest::Reconcile {
                    desired: target
                        .clone()
                        .with_ingress_routes(self.ingress_enabled(), &accepted_routes),
                },
                &observed,
            );
            self.check_plan(operation, &plan).await?;
            let action = plan.actions.iter().find(|action| match &action.kind {
                ActionKind::EnsureNetwork { .. } | ActionKind::EnsureVolume { .. } => true,
                ActionKind::EnsureService { service } => {
                    dependencies.contains(service.logical_name.as_str())
                }
                ActionKind::WaitForService { service } => target.services.iter().any(|desired| {
                    desired.name.as_str() == service
                        && dependencies.contains(desired.logical_name.as_str())
                }),
                _ => false,
            });
            let Some(action) = action else {
                return Ok(());
            };
            self.execute_action(action, operation, ownership, cancellation, deadline)
                .await?;
            if matches!(action.kind, ActionKind::WaitForService { .. }) {
                deadline = tokio::time::Instant::now() + self.retry.convergence_timeout;
            }
        }
    }

    /// Records one job run in build history, then removes its service unless
    /// the operation will resume it. The outcome is stored first, so a crash
    /// in between leaves a run that is resumed or removed, never rerun.
    async fn run_job(
        &self,
        operation: &Operation,
        job: &DesiredJobRun,
        ownership: &BTreeMap<String, String>,
        cancellation: &CancellationToken,
    ) -> Result<(), OperationError> {
        let name = job.logical_name.as_str();
        self.store
            .progress(&operation.id, "running_job", Some(name))
            .await?;
        let attempt = crate::build::BuildAttempt::start(
            Arc::clone(&self.store),
            &operation.application_id,
            &operation.id,
            job.container.logical_name.as_str(),
            &job.container.source.requested(),
            Some(name),
        )
        .await?;
        let journal = self
            .store
            .begin_action(Some(&operation.id), "run_job", Some(name))
            .await?;
        let result = self
            .execute_job(
                operation,
                job,
                ownership,
                cancellation,
                &journal,
                &attempt.log,
            )
            .await;
        self.store
            .finish_action(
                &journal,
                result.as_ref().err().map(OperationError::diagnostic),
            )
            .await?;
        // Failed means the job itself failed; any other error ended the
        // attempt before its outcome was known.
        let state = match &result {
            Ok(()) => BuildState::Succeeded,
            Err(OperationError::JobFailed { .. } | OperationError::JobTimeout(_)) => {
                BuildState::Failed
            }
            Err(_) => BuildState::Interrupted,
        };
        attempt
            .finish(state, Some(job.container.image.as_str()))
            .await?;
        if !self.resumable(operation, &result).await {
            // Removing the service also stops a run that timed out or was
            // superseded or cancelled.
            self.remove_job_run(operation, ownership).await;
        }
        result
    }

    /// Whether a retry of the operation may resume the run, so its service is
    /// kept: after a shutdown, or when the run's outcome is still unknown.
    async fn resumable(&self, operation: &Operation, result: &Result<(), OperationError>) -> bool {
        match result {
            Ok(())
            | Err(
                OperationError::JobFailed { .. }
                | OperationError::JobTimeout(_)
                | OperationError::Superseded,
            ) => false,
            Err(OperationError::Cancelled) => !matches!(
                self.check_current(operation).await,
                Err(OperationError::Superseded | OperationError::Cancelled)
            ),
            Err(_) => true,
        }
    }

    /// Starts the run unless it is running or already succeeded, waits for it,
    /// and records its output, exit code, and any Docker explanation of a
    /// failure.
    async fn execute_job(
        &self,
        operation: &Operation,
        job: &DesiredJobRun,
        ownership: &BTreeMap<String, String>,
        cancellation: &CancellationToken,
        journal: &crate::store::JournalAction,
        log: &BuildLog,
    ) -> Result<(), OperationError> {
        let secrets = self
            .service_secrets(&job.container.secrets, ownership)
            .await?;
        // A run Docker accepted before a lost response or a restart is resumed.
        // A failed run left by an earlier attempt is replaced: retrying reruns it.
        self.retry(operation, journal, cancellation, || async {
            if matches!(
                self.docker.job_status(job).await?,
                JobStatus::Running
                    | JobStatus::Finished {
                        exit_code: Some(0),
                        ..
                    }
            ) {
                return Ok(());
            }
            self.docker.ensure_swarm(false).await?;
            for (name, value) in &secrets {
                self.docker.ensure_secret(name, value, ownership).await?;
            }
            self.docker.start_job(job).await
        })
        .await?;
        let status = self.wait_job(operation, job, cancellation).await?;
        let truncated = status != JobStatus::Missing && self.record_output(job, log).await?;
        if let JobStatus::Finished {
            exit_code: Some(code),
            ..
        } = status
        {
            log.exit_code(code).await?;
        }
        let name = job.logical_name.to_string();
        let result = match status {
            JobStatus::Finished {
                exit_code: Some(0), ..
            } => Ok(()),
            JobStatus::Finished { exit_code, error } => {
                if let Some(error) = error {
                    Self::note(log, &error).await?;
                }
                Err(OperationError::JobFailed {
                    job: name,
                    exit_code,
                })
            }
            JobStatus::Missing => {
                Self::note(log, "the job's service was removed before it finished").await?;
                Err(OperationError::JobFailed {
                    job: name,
                    exit_code: None,
                })
            }
            JobStatus::Running => Err(OperationError::JobTimeout(name)),
        };
        // Marked last: a truncated log accepts no further notes.
        if truncated {
            log.truncated().await?;
        }
        result
    }

    /// Appends the run's output to its log and returns whether some of it was
    /// dropped. Unavailable output is noted instead of failing the job.
    async fn record_output(
        &self,
        job: &DesiredJobRun,
        log: &BuildLog,
    ) -> Result<bool, OperationError> {
        match self.docker.job_output(job).await {
            Ok(output) => {
                for (stream, bytes) in output.chunks {
                    log.append(&bytes, stream).await?;
                }
                Ok(output.truncated)
            }
            Err(error) => {
                tracing::warn!(error = ?error, job = %job.logical_name, "job output unavailable");
                Self::note(log, &format!("job output unavailable: {error}")).await?;
                Ok(false)
            }
        }
    }

    /// Appends a piqueld explanation to the run's output as a stderr line.
    async fn note(log: &BuildLog, message: &str) -> Result<(), OperationError> {
        log.append(
            format!("piqueld: {message}\n").as_bytes(),
            LogStream::Stderr,
        )
        .await
        .map_err(OperationError::from)
    }

    /// Polls until the run stops and returns its status, or `Running` once the
    /// job's timeout elapsed. When Docker could not report the status up to
    /// the timeout, its error is returned instead: the outcome is unknown, so
    /// the run is kept and the retried operation resumes it with a fresh
    /// timeout, as after a daemon restart.
    async fn wait_job(
        &self,
        operation: &Operation,
        job: &DesiredJobRun,
        cancellation: &CancellationToken,
    ) -> Result<JobStatus, OperationError> {
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(u64::from(job.timeout_seconds));
        let mut failure = None;
        loop {
            self.check_current(operation).await?;
            let status = tokio::select! {
                () = cancellation.cancelled() => return Err(OperationError::Cancelled),
                () = tokio::time::sleep_until(deadline) => {
                    return failure.map_or(Ok(JobStatus::Running), |error| Err(OperationError::from(error)));
                }
                status = self.docker.job_status(job) => status,
            };
            match status {
                Ok(JobStatus::Running) => failure = None,
                Ok(status) => return Ok(status),
                Err(error) => {
                    tracing::warn!(error = ?error, job = %job.logical_name, "job status check failed; retrying");
                    failure = Some(error);
                }
            }
            tokio::select! {
                () = cancellation.cancelled() => return Err(OperationError::Cancelled),
                () = tokio::time::sleep(JOB_POLL_INTERVAL) => {}
            }
        }
    }

    /// Journals removal of the selected job services like other runtime
    /// mutations. The operation must still be current.
    pub(super) async fn remove_jobs(
        &self,
        operation: &Operation,
        ownership: &BTreeMap<String, String>,
        runs: JobRuns<'_>,
        cancellation: &CancellationToken,
    ) -> Result<(), OperationError> {
        let journal = self
            .store
            .begin_action(Some(&operation.id), "remove_jobs", None)
            .await?;
        let result = self
            .retry(operation, &journal, cancellation, || {
                self.docker.remove_jobs(ownership, runs)
            })
            .await;
        self.store
            .finish_action(
                &journal,
                result.as_ref().err().map(OperationError::diagnostic),
            )
            .await?;
        result
    }

    /// Removes this operation's run once, even after the operation was
    /// superseded. Failures are only logged: the job's outcome is already
    /// recorded, and the next deployment or deletion removes leftovers.
    async fn remove_job_run(&self, operation: &Operation, ownership: &BTreeMap<String, String>) {
        let result: Result<(), OperationError> = async {
            let journal = self
                .store
                .begin_action(Some(&operation.id), "remove_jobs", None)
                .await?;
            let result = {
                let _guard = self.mutations.lock().await;
                self.store.action_request(&journal, 1).await?;
                self.docker
                    .remove_jobs(ownership, JobRuns::Of(&operation.id))
                    .await
                    .map_err(OperationError::from)
            };
            self.store
                .finish_action(
                    &journal,
                    result.as_ref().err().map(OperationError::diagnostic),
                )
                .await?;
            result
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(error = ?error, operation = %operation.id, "job service removal failed");
        }
    }
}
