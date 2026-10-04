//! One-shot jobs run after preparation and before promotion, so a failed job
//! leaves the active target and its running services untouched.
//!
//! Each job succeeds at most once per operation. Retries and restarts skip jobs
//! with a recorded success, and resume a run still present in Docker instead
//! of starting it again.
use super::{
    ActionKind, CancellationToken, Controller, DockerApi, Duration, Operation, OperationError,
    Plan, PlanRequest,
};
use crate::{
    build::BuildLog,
    docker::{JobRuns, JobStatus},
};
use piqueld_core::{
    DesiredJob, ResolvedApplication,
    api::{BuildState, LogStream},
    manifest::JobRun,
};
use std::{collections::BTreeMap, sync::Arc};

/// Delay between job progress checks.
const JOB_POLL_INTERVAL: Duration = Duration::from_secs(1);

impl<D: DockerApi> Controller<D> {
    /// Runs before-rollout jobs in declared order. Promoted operations already
    /// passed this point, so their retries and repairs never rerun jobs.
    pub(super) async fn run_jobs(
        &self,
        operation: &Operation,
        target: &ResolvedApplication,
        cancellation: &CancellationToken,
    ) -> Result<(), OperationError> {
        if target.jobs.is_empty() || self.store.is_promoted(&operation.id).await? {
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
        if jobs.is_empty() {
            return Ok(());
        }
        let ownership = self.ownership_labels(&operation.application_id);
        // A run left by an earlier operation must not overlap this one's jobs.
        self.remove_jobs(
            operation,
            &ownership,
            JobRuns::Except(&operation.id),
            cancellation,
        )
        .await?;
        // Jobs join the private network and mount the target's volumes, which a
        // first deployment has not created yet. Services are not touched, and a
        // blocked deployment runs no jobs because it could never roll out.
        let deadline = tokio::time::Instant::now() + self.retry.convergence_timeout;
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
        for action in plan.actions.iter().filter(|action| {
            matches!(
                action.kind,
                ActionKind::EnsureNetwork { .. } | ActionKind::EnsureVolume { .. }
            )
        }) {
            self.execute_action(action, operation, &ownership, cancellation, deadline)
                .await?;
        }
        for job in &jobs {
            self.run_job(operation, job, &ownership, cancellation)
                .await?;
        }
        Ok(())
    }

    /// Records one job run in build history and removes its service afterwards.
    /// On shutdown the run keeps going so the restarted operation resumes it.
    async fn run_job(
        &self,
        operation: &Operation,
        job: &DesiredJob,
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
        let shutdown = matches!(result, Err(OperationError::Cancelled))
            && !matches!(
                self.check_current(operation).await,
                Err(OperationError::Superseded | OperationError::Cancelled)
            );
        if !shutdown {
            // Removing the service also stops a run that timed out or was
            // superseded or cancelled.
            self.remove_job_run(operation, ownership).await;
        }
        let state = match &result {
            Ok(()) => BuildState::Succeeded,
            Err(OperationError::Cancelled | OperationError::Superseded) => BuildState::Interrupted,
            Err(_) => BuildState::Failed,
        };
        attempt
            .finish(state, Some(job.container.image.as_str()))
            .await?;
        result
    }

    /// Starts the run unless it already exists, waits for it, and records its
    /// output, exit code, and any Docker explanation of a failure.
    async fn execute_job(
        &self,
        operation: &Operation,
        job: &DesiredJob,
        ownership: &BTreeMap<String, String>,
        cancellation: &CancellationToken,
        journal: &crate::store::JournalAction,
        log: &BuildLog,
    ) -> Result<(), OperationError> {
        let secrets = self
            .service_secrets(&job.container.secrets, ownership)
            .await?;
        // A run Docker accepted before a lost response or a restart is resumed.
        self.retry(operation, journal, cancellation, || async {
            if self.docker.job_status(job).await? != JobStatus::Missing {
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
        if status != JobStatus::Missing {
            match self.docker.job_output(job).await {
                Ok(output) => {
                    for (stream, bytes) in output {
                        log.append(&bytes, stream).await?;
                    }
                }
                Err(error) => {
                    tracing::warn!(error = ?error, job = %job.logical_name, "job output unavailable");
                    Self::note(log, &format!("job output unavailable: {error}")).await?;
                }
            }
        }
        if let JobStatus::Finished {
            exit_code: Some(code),
            ..
        } = status
        {
            log.exit_code(code).await?;
        }
        let job = job.logical_name.to_string();
        match status {
            JobStatus::Finished {
                exit_code: Some(0), ..
            } => Ok(()),
            JobStatus::Finished { exit_code, error } => {
                if let Some(error) = error {
                    Self::note(log, &error).await?;
                }
                Err(OperationError::JobFailed { job, exit_code })
            }
            JobStatus::Missing => {
                Self::note(log, "the job's service was removed before it finished").await?;
                Err(OperationError::JobFailed {
                    job,
                    exit_code: None,
                })
            }
            JobStatus::Running => Err(OperationError::JobTimeout(job)),
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
    /// job's timeout elapsed. Docker errors are retried until the deadline and
    /// then end as a timeout, which is never retried automatically. A daemon
    /// restart resumes the run with a fresh timeout.
    async fn wait_job(
        &self,
        operation: &Operation,
        job: &DesiredJob,
        cancellation: &CancellationToken,
    ) -> Result<JobStatus, OperationError> {
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(u64::from(job.timeout_seconds));
        loop {
            self.check_current(operation).await?;
            let status = tokio::select! {
                () = cancellation.cancelled() => return Err(OperationError::Cancelled),
                () = tokio::time::sleep_until(deadline) => return Ok(JobStatus::Running),
                status = self.docker.job_status(job) => status,
            };
            match status {
                Ok(JobStatus::Running) => {}
                Ok(status) => return Ok(status),
                Err(error) => {
                    tracing::warn!(error = ?error, job = %job.logical_name, "job status check failed; retrying");
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
    /// decided, and the next deployment with jobs or deletion removes leftovers.
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
