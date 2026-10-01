//! One-shot jobs run after preparation and before promotion, so a failed job
//! leaves the active target and its running services untouched.
use super::{
    ActionKind, CancellationToken, Controller, DockerApi, Duration, Operation, OperationError,
    Plan, PlanRequest,
};
use crate::{build::BuildLog, docker::JobStatus};
use piqueld_core::{DesiredJob, ResolvedApplication, api::BuildState, manifest::JobRun};
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
        let jobs = target
            .jobs
            .iter()
            .filter(|job| job.run == JobRun::BeforeRollout)
            .collect::<Vec<_>>();
        if jobs.is_empty() || self.store.is_promoted(&operation.id).await? {
            return Ok(());
        }
        let ownership = self.ownership_labels(&operation.application_id);
        // Jobs join the private network and mount the target's volumes, which a
        // first deployment has not created yet. Services are not touched.
        let deadline = tokio::time::Instant::now() + self.retry.convergence_timeout;
        let observed = self
            .observe_with_retry(operation, cancellation, deadline)
            .await?;
        let plan = Plan::from_request(
            &PlanRequest::Reconcile {
                desired: target.clone(),
            },
            &observed,
        );
        for action in plan.actions.iter().filter(|action| {
            matches!(
                action.kind,
                ActionKind::EnsureNetwork { .. } | ActionKind::EnsureVolume { .. }
            )
        }) {
            self.execute_action(action, operation, &ownership, cancellation, deadline)
                .await?;
        }
        for job in jobs {
            self.run_job(operation, job, &ownership, cancellation)
                .await?;
        }
        Ok(())
    }

    /// Records one job run in build history and removes its service afterwards.
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
        if cancellation.is_cancelled() {
            // Shutdown leaves the job running; the next run replaces it and
            // dropping the attempt records it as interrupted.
            return result;
        }
        // Removing the service also stops a job that timed out or was superseded.
        let cleanup = self.remove_jobs(operation, ownership, cancellation).await;
        let state = match &result {
            Ok(()) => BuildState::Succeeded,
            Err(OperationError::Cancelled | OperationError::Superseded) => BuildState::Interrupted,
            Err(_) => BuildState::Failed,
        };
        attempt
            .finish(state, Some(job.container.image.as_str()))
            .await?;
        result.and(cleanup)
    }

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
        self.retry(operation, journal, cancellation, || async {
            self.docker.ensure_swarm(false).await?;
            for (name, value) in &secrets {
                self.docker.ensure_secret(name, value, ownership).await?;
            }
            self.docker.start_job(job).await
        })
        .await?;
        self.wait_job(operation, job, cancellation, log).await
    }

    /// Polls until the job finishes, recording its output. Docker failures are
    /// retried until the job's own deadline, then reported instead of a timeout.
    async fn wait_job(
        &self,
        operation: &Operation,
        job: &DesiredJob,
        cancellation: &CancellationToken,
        log: &BuildLog,
    ) -> Result<(), OperationError> {
        let name = job.logical_name.to_string();
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(u64::from(job.timeout_seconds));
        let mut failure = None;
        loop {
            self.check_current(operation).await?;
            let status = tokio::select! {
                () = cancellation.cancelled() => return Err(OperationError::Cancelled),
                () = tokio::time::sleep_until(deadline) => {
                    return Err(failure.map_or(OperationError::JobTimeout(name), OperationError::from));
                }
                status = self.docker.job_status(job) => status,
            };
            match status {
                Ok(JobStatus::Finished { exit_code, output }) => {
                    for (stream, bytes) in output {
                        log.append(&bytes, stream).await?;
                    }
                    if let Some(code) = exit_code {
                        log.exit_code(code).await?;
                    }
                    return if exit_code == Some(0) {
                        Ok(())
                    } else {
                        Err(OperationError::JobFailed {
                            job: name,
                            exit_code,
                        })
                    };
                }
                Ok(JobStatus::Running) => failure = None,
                Err(error) => {
                    tracing::warn!(error = ?error, job = %name, "job status check failed; retrying");
                    failure = Some(error);
                }
            }
            tokio::select! {
                () = cancellation.cancelled() => return Err(OperationError::Cancelled),
                () = tokio::time::sleep(JOB_POLL_INTERVAL) => {}
            }
        }
    }

    /// Journals job service removal like other runtime mutations.
    pub(super) async fn remove_jobs(
        &self,
        operation: &Operation,
        ownership: &BTreeMap<String, String>,
        cancellation: &CancellationToken,
    ) -> Result<(), OperationError> {
        let journal = self
            .store
            .begin_action(Some(&operation.id), "remove_jobs", None)
            .await?;
        let result = self
            .retry(operation, &journal, cancellation, || {
                self.docker.remove_jobs(ownership)
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
}
