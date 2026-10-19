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
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

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
        let ownership = self.ownership_labels(&operation.environment_id);
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
            // A completed run must stop before another job of this operation
            // starts: cleanup selects runs by operation ID.
            self.retry_job_cleanup(&operation.environment_id).await?;
            let job = self
                .prepare_job_dependencies(operation, target, job, &ownership, cancellation)
                .await?;
            self.run_job(operation, &job, &ownership, cancellation)
                .await?;
        }
        Ok(())
    }

    /// Uses the full rollout plan to check blockers, but executes only shared
    /// infrastructure, this job's startup dependencies, and the removal of
    /// services sharing its exclusive volumes. Replanning preserves dependency
    /// ordering without promoting the candidate or touching other services.
    /// Each dependency receives its own convergence budget. Returns the run
    /// as last planned, joined to the links its environment imports.
    async fn prepare_job_dependencies(
        &self,
        operation: &Operation,
        target: &ResolvedApplication,
        job: &DesiredJobRun,
        ownership: &BTreeMap<String, String>,
        cancellation: &CancellationToken,
    ) -> Result<DesiredJobRun, OperationError> {
        let mut deadline = tokio::time::Instant::now() + self.retry.convergence_timeout;
        loop {
            let observed = self
                .observe_with_retry(operation, cancellation, deadline)
                .await?;
            let accepted_routes = self.store.applied_routes(&operation.environment_id).await?;
            let desired = self
                .runtime_target(target, &accepted_routes, &observed)
                .await?;
            let projected = desired
                .jobs
                .iter()
                .find(|projected| projected.logical_name == job.logical_name)
                .map(|projected| projected.for_operation(job.operation()));
            let plan = Plan::from_request(&PlanRequest::Reconcile { desired }, &observed);
            self.check_plan(operation, &plan).await?;
            let Some(action) = plan.next_job_preparation(target, job, &observed) else {
                return Ok(projected.unwrap_or_else(|| job.clone()));
            };
            self.execute_action(&action, operation, ownership, cancellation, deadline)
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
            &operation.environment_id,
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
        // Failed means the job itself failed; any other error ended the
        // attempt before its outcome was known.
        let state = match &result {
            Ok(()) => BuildState::Succeeded,
            Err(OperationError::JobFailed { .. } | OperationError::JobTimeout(_)) => {
                BuildState::Failed
            }
            Err(_) => BuildState::Interrupted,
        };
        let finished = attempt
            .finish(state, Some(job.container.image.as_str()))
            .await;
        // Keep an unrecorded success available for resumption. A timeout or
        // cancellation must still stop even if bookkeeping failed.
        let stop =
            !self.resumable(operation, &result).await && (finished.is_ok() || result.is_err());
        let queued = if stop {
            self.store.request_job_cleanup(&operation.id).await
        } else {
            Ok(())
        };
        let journaled = self
            .store
            .finish_action(
                &journal,
                result.as_ref().err().map(OperationError::diagnostic),
            )
            .await;
        if stop {
            // Removing the service also stops a run that timed out or was
            // superseded or cancelled.
            self.remove_job_run(operation, ownership).await;
        }
        for recorded in [finished, queued, journaled] {
            if let Err(error) = recorded {
                let error = OperationError::from(error);
                if result.is_ok() {
                    return Err(error);
                }
                // Retain the terminal job error so retrying bookkeeping never
                // reruns a timed-out or failed migration.
                self.store
                    .report_diagnostic(&error.diagnostic(), Some(&operation.environment_id))
                    .await;
            }
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
        // An exclusive run starts once the services it stopped let go of its
        // volumes; a newer deployment stops the wait. A run to resume holds
        // them itself.
        if job.exclusive_volumes().next().is_some() {
            let resumable = AtomicBool::new(false);
            self.retry(operation, journal, cancellation, || async {
                let status = self.docker.job_status(job).await?;
                resumable.store(status.resumable(), Ordering::Relaxed);
                Ok(())
            })
            .await?;
            if !resumable.load(Ordering::Relaxed) {
                tokio::select! {
                    () = cancellation.cancelled() => return Err(OperationError::Cancelled),
                    released = self.wait_unlocked(self.docker.wait_volumes_released(job)) => {
                        released?;
                    }
                }
            }
        }
        // A run Docker accepted before a lost response or a restart is resumed.
        // A failed run left by an earlier attempt is replaced: retrying reruns it.
        self.retry(operation, journal, cancellation, || async {
            if self.docker.job_status(job).await?.resumable() {
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
            JobStatus::Running { .. } => Err(OperationError::JobTimeout(name)),
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
        let line = format!("piqueld: {message}\n");
        Ok(log.append(line.as_bytes(), LogStream::Stderr).await?)
    }

    /// Polls until the run stops and returns its status, or `Running` once the
    /// run's deadline passed. The deadline was fixed when the run started, so
    /// resuming after a restart never extends it; runs started without one
    /// get the job's full timeout from now. When Docker could not report the
    /// status up to the deadline, its error is returned instead: the outcome
    /// is unknown, so the run is kept and the retried operation resumes it.
    async fn wait_job(
        &self,
        operation: &Operation,
        job: &DesiredJobRun,
        cancellation: &CancellationToken,
    ) -> Result<JobStatus, OperationError> {
        let mut deadline =
            tokio::time::Instant::now() + Duration::from_secs(u64::from(job.timeout_seconds));
        let mut failure = None;
        loop {
            self.check_current(operation).await?;
            let status = tokio::select! {
                () = cancellation.cancelled() => return Err(OperationError::Cancelled),
                () = tokio::time::sleep_until(deadline) => {
                    return failure.map_or(Ok(JobStatus::Running { deadline: None }), |error| Err(OperationError::from(error)));
                }
                status = self.docker.job_status(job) => status,
            };
            match status {
                Ok(JobStatus::Running { deadline: started }) => {
                    failure = None;
                    if let Some(started) = started {
                        let remaining = started
                            .duration_since(std::time::SystemTime::now())
                            .unwrap_or_default();
                        deadline = tokio::time::Instant::now() + remaining;
                    }
                }
                Ok(status) => return Ok(status),
                Err(error) => {
                    tracing::warn!(error = ?error, job = %job.logical_name, "job status check failed; retrying");
                    failure = Some(error);
                }
            }
            Self::pause(cancellation, JOB_POLL_INTERVAL).await?;
        }
    }

    /// Journals removal of the selected job services like other runtime
    /// mutations, then waits for their containers to stop. The operation must
    /// still be current.
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
        let result = async {
            self.retry(operation, &journal, cancellation, || {
                self.docker.remove_jobs(ownership, runs)
            })
            .await?;
            self.wait_unlocked(self.docker.wait_jobs_stopped(ownership, runs))
                .await
        }
        .await;
        self.close_action(&journal, result).await
    }

    /// Waits for `released`, which only reads Docker, for at most the
    /// convergence timeout and outside the mutation lock, so Docker writes
    /// for every environment carry on meanwhile. That is safe: a job starts,
    /// and a service is written, only under the lock, after checking that no
    /// run or service still holds a volume it would share.
    async fn wait_unlocked(
        &self,
        released: impl Future<Output = Result<(), crate::docker::DockerError>>,
    ) -> Result<(), OperationError> {
        tokio::time::timeout(self.retry.convergence_timeout, released)
            .await
            .map_err(|_| OperationError::ConvergenceTimeout)?
            .map_err(OperationError::from)
    }

    /// Retries durable cleanup independently of deployment execution. Once
    /// runs are removed, so are the configs of files only they mounted; a
    /// failed removal is logged and left to the next operation.
    pub(super) async fn retry_job_cleanup(
        &self,
        application: &piqueld_core::EnvironmentId,
    ) -> Result<(), OperationError> {
        let ownership = self.ownership_labels(application);
        let pending = self.store.pending_job_cleanup(application).await?;
        for operation in &pending {
            self.stop_job_run(operation, &ownership).await?;
        }
        if !pending.is_empty() {
            let _guard = self.mutations.lock().await;
            if let Err(error) = self.docker.prune_files(&ownership).await {
                tracing::warn!(?error, "removing unused service files failed");
            }
        }
        Ok(())
    }

    /// Attempts immediate cleanup even after bookkeeping failed. Failed removal
    /// leaves durable intent for subsequent scans, including after a restart.
    async fn remove_job_run(&self, operation: &Operation, ownership: &BTreeMap<String, String>) {
        if let Err(error) = self.stop_job_run(&operation.id, ownership).await {
            tracing::warn!(error = ?error, operation = %operation.id, "job service removal failed; cleanup will be retried");
        }
    }

    /// Removes the operation's job run under the mutation lock, waits outside
    /// it until the run stopped, and only then clears its cleanup intent. An
    /// environment's runs start only from its own scan, which retries its
    /// pending cleanup first, so no run of the operation starts meanwhile.
    async fn stop_job_run(
        &self,
        operation: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), OperationError> {
        self.journaled(operation, "remove_jobs", None, async {
            {
                let _guard = self.mutations.lock().await;
                self.docker
                    .remove_jobs(ownership, JobRuns::Of(operation))
                    .await?;
            }
            self.wait_unlocked(
                self.docker
                    .wait_jobs_stopped(ownership, JobRuns::Of(operation)),
            )
            .await
        })
        .await?;
        Ok(self.store.finish_job_cleanup(operation).await?)
    }
}
