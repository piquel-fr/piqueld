use super::{
    ApplicationState, Arc, BlockedPlan, CancellationToken, Controller, DockerApi, Duration, Notify,
    Operation, OperationError, OperationKind, OperationState, Plan, PlanRequest, StoreError,
    StoredEnvironment,
};
use futures_util::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use piqueld_core::{EnvironmentId, observability::DiagnosticCode};
use std::collections::{HashMap, HashSet};
use tracing::Instrument;

/// Result of a discovery pass: whether it was a full scan, and each selected
/// environment with its latest operation.
type Discovered = (bool, Vec<(StoredEnvironment, Operation)>);
/// Application/diagnostic-code pairs already recorded during one discovery pass.
type ScanFailures = Arc<tokio::sync::Mutex<HashSet<(EnvironmentId, String)>>>;
/// Time running operations get on shutdown to record their outcome.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

impl<D: DockerApi> Controller<D> {
    /// One event loop polls environment futures and discovery concurrently. No
    /// environment holds the loop while waiting for Docker, `SQLite`, or a timer.
    /// Failures are recorded inside each future, never in the loop body: a job
    /// suspended while holding the writer or a failure set must keep being polled.
    /// Process startup closes abandoned actions before any worker starts; this
    /// loop runs beside ingress, whose daemon actions it must not interrupt.
    ///
    /// # Panics
    /// Panics if the scan interval is zero.
    ///
    /// # Errors
    /// Store failures are logged and retried. Shutdown cancels running
    /// operations and gives them [`SHUTDOWN_GRACE`] to record their outcome,
    /// such as a job run's result, instead of dropping them mid-way. Health
    /// refreshes and discovery hold no job run and are dropped first, so
    /// neither holds a lock the operations need; startup closes whatever
    /// any of them left open.
    pub async fn run(
        &self,
        wake: Arc<Notify>,
        interval: Duration,
        finished_operation_days: u64,
        event_days: u64,
        cancellation: CancellationToken,
    ) -> Result<(), StoreError> {
        let mut jobs = FuturesUnordered::new();
        let mut active = HashMap::new();
        let mut health_active = HashSet::new();
        let mut health_jobs = FuturesUnordered::new();
        let mut discovery: Option<BoxFuture<'_, Option<Discovered>>> = None;
        let mut tick = tokio::time::interval(Duration::from_secs(1).min(interval));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut next_scan = tokio::time::Instant::now();
        let mut recovered = false;
        let mut requested = true;
        loop {
            if requested && discovery.is_none() {
                let full = tokio::time::Instant::now() >= next_scan;
                if full {
                    next_scan = tokio::time::Instant::now() + interval;
                }
                discovery = Some(
                    self.discovery_pass(!recovered, full, finished_operation_days, event_days)
                        .boxed(),
                );
                requested = false;
            }
            let discovered = async {
                match discovery.as_mut() {
                    Some(discovery) => discovery.await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                () = cancellation.cancelled() => {
                    // Unpolled, they could hold the mutation lock or store writer.
                    drop((health_jobs, discovery));
                    Self::finish_on_shutdown(jobs).await;
                    return Ok(());
                }
                () = wake.notified() => requested = true,
                () = self.relink.notified() => {
                    next_scan = tokio::time::Instant::now();
                    requested = true;
                }
                _ = tick.tick() => requested = true,
                result = discovered => {
                    discovery = None;
                    if let Some((full, applications)) = result {
                        recovered = true;
                        let failures = ScanFailures::default();
                        for (application, operation) in applications {
                            let id = application.id().clone();
                            // Full scans refresh health and enforce links, with at most one
                            // job per application.
                            if full && health_active.insert(id.clone()) {
                                let span = tracing::debug_span!(
                                    "application_health",
                                    environment_id = %id,
                                    operation_id = %operation.id,
                                    generation = application.application.generation,
                                );
                                let health = self.refresh_health(
                                    id.clone(),
                                    operation.id.clone(),
                                    application.application.generation,
                                    Arc::clone(&failures),
                                );
                                health_jobs.push(health.instrument(span));
                            }
                            // A running job is cancelled if a newer operation replaced its own;
                            // its completion requests another pass that picks up the new one.
                            if let Some((current, token)) = active.get(&id) {
                                let token: &CancellationToken = token;
                                if current != &operation.id {
                                    token.cancel();
                                }
                                continue;
                            }
                            let token = cancellation.child_token();
                            active.insert(id, (operation.id.clone(), token.clone()));
                            // A deletion unlinks the environment as soon as it is requested;
                            // only its first run asks the other side to replan, not retries.
                            if operation.kind == OperationKind::Delete
                                && operation.state == OperationState::Requested
                                && application
                                    .resolved
                                    .as_ref()
                                    .is_some_and(piqueld_core::ResolvedApplication::has_links)
                            {
                                self.relink.notify_one();
                            }
                            jobs.push(self.scan_job(
                                application,
                                operation,
                                token,
                                Arc::clone(&failures),
                            ));
                        }
                    }
                }
                Some(id) = health_jobs.next(), if !health_jobs.is_empty() => {
                    health_active.remove(&id);
                }
                Some(id) = jobs.next(), if !jobs.is_empty() => {
                    active.remove(&id);
                    requested = true;
                }
            }
        }
    }

    /// Waits up to [`SHUTDOWN_GRACE`] for the running operation `jobs`, whose
    /// tokens shutdown cancelled, to stop at their next cancellation check and
    /// record their outcome; drops the rest.
    async fn finish_on_shutdown(jobs: FuturesUnordered<impl Future<Output = EnvironmentId>>) {
        if tokio::time::timeout(SHUTDOWN_GRACE, jobs.for_each(|_| async {}))
            .await
            .is_err()
        {
            tracing::warn!("operations still running at shutdown were dropped");
        }
    }

    /// Repairs the active target of environment `id`, records its fresh
    /// health and enforces its links, reporting failures as scan
    /// diagnostics. A failed repair must not keep a revoked link open, so
    /// links are enforced regardless. Resolves to `id`.
    async fn refresh_health(
        &self,
        id: EnvironmentId,
        operation_id: String,
        generation: u64,
        failures: ScanFailures,
    ) -> EnvironmentId {
        let maintained = self.maintain_active(&id, &operation_id).await;
        let reported = async {
            let observed = self.docker.observe(&id).await?;
            self.store.record_health(&operation_id, &observed).await?;
            self.enforce_links(&id, &operation_id, &observed).await?;
            self.enforce_telemetry(&id).await
        }
        .await;
        for error in [maintained.err(), reported.err()].into_iter().flatten() {
            if let Err(report_error) = self
                .record_scan_diagnostic(&id, &error.diagnostic(), &failures)
                .await
            {
                tracing::error!(environment_id = %id, error = ?report_error, "health diagnostic could not be persisted");
            }
            tracing::warn!(environment_id = %id, operation_id = %operation_id, generation, %error, "health reporting failed");
        }
        id
    }

    /// Scans `application` and its discovered `operation` until `token`
    /// cancels it, reporting a journal failure as a diagnostic, then asks the
    /// environments it links to to replan when its links changed. Resolves to
    /// the environment's ID.
    async fn scan_job(
        &self,
        application: StoredEnvironment,
        operation: Operation,
        token: CancellationToken,
        failures: ScanFailures,
    ) -> EnvironmentId {
        if let Err(error) =
            Box::pin(self.scan_application(&application, &operation, &token, &failures)).await
        {
            let failure = OperationError::Journal(error);
            self.store
                .report_diagnostic(&failure.diagnostic(), Some(application.id()))
                .await;
        }
        if self.relinked(&application).await {
            self.relink.notify_one();
        }
        application.id().clone()
    }

    /// Whether the exports or imports `before` deployed changed since, so the
    /// environments it links to must replan now rather than at the next full
    /// scan. Read failures count as unchanged; the next full scan replans anyway.
    async fn relinked(&self, before: &StoredEnvironment) -> bool {
        let links = |target: Option<&piqueld_core::ResolvedApplication>| {
            target.map(|target| (target.exports.clone(), target.imports.clone()))
        };
        self.store
            .get(before.id())
            .await
            .is_ok_and(|after| links(after.resolved.as_ref()) != links(before.resolved.as_ref()))
    }

    /// Recovers interrupted work on the first pass, prunes history on full
    /// scans, and finds environments to process. Failures are reported here.
    async fn discovery_pass(
        &self,
        recover: bool,
        full: bool,
        finished_operation_days: u64,
        event_days: u64,
    ) -> Option<Discovered> {
        let result = async {
            if recover {
                self.store.recover_interrupted().await?;
                self.store.recover_builds().await?;
            }
            if full {
                self.prune_history(finished_operation_days, event_days)
                    .await?;
            }
            self.discover(full).await
        }
        .await;
        match result {
            Ok(applications) => Some((full, applications)),
            Err(error) => {
                let failure = OperationError::Journal(error);
                self.store
                    .report_diagnostic(&failure.diagnostic(), None)
                    .await;
                None
            }
        }
    }

    /// Selects the environments needing work among all of them. A full scan
    /// selects every environment with an operation; otherwise only requested,
    /// cleanly running, or retry-due operations are selected. Reads every
    /// latest operation at once, as discovery runs every second.
    async fn discover(
        &self,
        full: bool,
    ) -> Result<Vec<(StoredEnvironment, Operation)>, StoreError> {
        let mut latest = self.store.latest_operations().await?;
        let mut applications = Vec::new();
        for app in self.store.every_environment().await? {
            if let Some(operation) = latest.remove(app.id())
                && (full
                    || operation.state == OperationState::Requested
                    || (operation.state == OperationState::Running
                        && operation.error_code.is_none())
                    || Self::retry_due(&operation))
            {
                applications.push((app, operation));
            }
        }
        Ok(applications)
    }

    /// Runs one concurrent observation/reconciliation pass, also used by runtime
    /// tests. Each environment then enforces its links and telemetry socket,
    /// as full scans do.
    ///
    /// # Errors
    /// Returns a store error if environments cannot be discovered or processed.
    pub async fn scan(&self, cancellation: &CancellationToken) -> Result<(), StoreError> {
        let applications = self.discover(true).await?;
        let mut jobs = FuturesUnordered::new();
        let failures = ScanFailures::default();
        for (application, operation) in applications {
            let scan_failures = Arc::clone(&failures);
            jobs.push(async move {
                Box::pin(self.scan_application(
                    &application,
                    &operation,
                    cancellation,
                    &scan_failures,
                ))
                .await?;
                let enforced = async {
                    let observed = self.docker.observe(application.id()).await?;
                    self.enforce_links(application.id(), &operation.id, &observed)
                        .await?;
                    self.enforce_telemetry(application.id()).await
                }
                .await;
                match enforced {
                    Ok(()) => Ok(()),
                    Err(OperationError::Journal(error)) => Err(error),
                    Err(error) => {
                        self.record_scan_diagnostic(
                            application.id(),
                            &error.diagnostic(),
                            &scan_failures,
                        )
                        .await
                    }
                }
            });
        }
        while let Some(result) = jobs.next().await {
            result?;
        }
        Ok(())
    }

    /// Whether a transiently failed operation has waited out its backoff.
    /// Only codes diagnostic policy marks retryable qualify; the delay doubles
    /// per consecutive failure from 5s and is capped at 60s.
    ///
    /// ```text
    /// failures: 1   2    3    4    5+
    /// delay:    5s  10s  20s  40s  60s
    /// ```
    fn retry_due(operation: &Operation) -> bool {
        if !Self::failure(operation).is_some_and(DiagnosticCode::retryable) {
            return false;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let delay =
            (5_u128 << operation.consecutive_failures.saturating_sub(1).min(4)).min(60) * 1000;
        now >= u128::try_from(operation.updated_at_ms)
            .unwrap_or(u128::MAX)
            .saturating_add(delay)
    }

    /// The classification of the operation's recorded failure, if any.
    fn failure(operation: &Operation) -> Option<DiagnosticCode> {
        DiagnosticCode::parse(operation.error_code.as_deref()?)
    }

    /// Whether a failed job ended the operation's deployment before promotion.
    fn job_failed(operation: &Operation) -> bool {
        matches!(
            Self::failure(operation),
            Some(DiagnosticCode::JobFailed | DiagnosticCode::JobTimeout)
        )
    }

    /// Whether a failure persists even when services match the target: ingress
    /// still awaits publication, or a failed job ended the deployment before
    /// promotion.
    fn failure_outlives_converged_plan(operation: &Operation) -> bool {
        Self::job_failed(operation)
            || Self::failure(operation) == Some(DiagnosticCode::IngressUnavailable)
    }

    /// Retries pending cleanup before processing `operation`, the latest
    /// operation discovery found for the environment. Cleanup alone never
    /// reopens a terminal job failure. Normal execution still runs when due,
    /// enforcing its backoff and cleanup fence.
    #[tracing::instrument(skip_all, fields(environment_id = %application.id(), operation_id = %operation.id, generation = application.application.generation))]
    async fn scan_application(
        &self,
        application: &StoredEnvironment,
        operation: &Operation,
        cancellation: &CancellationToken,
        failures: &ScanFailures,
    ) -> Result<(), StoreError> {
        if let Err(error) = self.retry_job_cleanup(application.id()).await {
            self.record_scan_diagnostic(application.id(), &error.diagnostic(), failures)
                .await?;
        }
        self.scan_latest_operation(application, operation, cancellation, failures)
            .await
    }

    /// Processes `discovered`, the operation discovery found, never one
    /// requested since: the coordinator tracks and cancels this scan by its
    /// ID, and the next pass picks up newer operations. Its state is read
    /// again first, since an earlier scan may have finished it after
    /// discovery read it. Every transition rechecks that it is still the
    /// latest, so a superseded one changes nothing.
    ///
    /// 1. Repairs drift in the active target while a newer target is unpromoted.
    /// 2. Runs requested, cleanly running, or retry-due operations.
    /// 3. Otherwise plans the latest target against fresh observations: blocked
    ///    plans degrade the environment, converged plans mark it ready, and drift
    ///    after success (or a cleared permanent blocker) reopens the operation.
    ///
    /// Observation failures are recorded as deduplicated diagnostics, not errors.
    async fn scan_latest_operation(
        &self,
        application: &StoredEnvironment,
        discovered: &Operation,
        cancellation: &CancellationToken,
        failures: &ScanFailures,
    ) -> Result<(), StoreError> {
        let latest = &match self.store.operation(&discovered.id).await {
            // Deleted with its environment since.
            Err(StoreError::NotFound) => return Ok(()),
            operation => operation?,
        };
        self.repair_before_execution(application, &latest.id, failures)
            .await?;
        let running = latest.state == OperationState::Running;
        if latest.state == OperationState::Requested
            || (running && (latest.error_code.is_none() || Self::retry_due(latest)))
        {
            return Box::pin(self.run_operation(latest, cancellation)).await;
        }
        if Self::retry_due(latest) {
            // Reopens it only while it is still the latest and terminal.
            if let Some(operation) = self
                .store
                .request_reconcile(application.id(), &latest.id)
                .await?
            {
                Box::pin(self.run_operation(&operation, cancellation)).await?;
            }
            return Ok(());
        }
        let application = self.store.get(application.id()).await?;
        let observed = match self.docker.observe(application.id()).await {
            Ok(observed) => observed,
            Err(error) => {
                self.record_scan_diagnostic(
                    application.id(),
                    &OperationError::from(error).diagnostic(),
                    failures,
                )
                .await?;
                return Ok(());
            }
        };
        // An older resolved target is useful for reporting health, but cannot
        // authorize corrective work once newer intent has been accepted.
        let prepared = self.store.prepared_target(&latest.id).await?;
        let target = prepared.as_ref().or(application.resolved.as_ref());
        let request = if application.delete_intent() {
            self.delete_request(application.id())
        } else if let Some(target) = target {
            let stopped = self.store.is_stopped(application.id()).await?;
            PlanRequest::Reconcile {
                desired: self.runtime_target(target, &[], &observed, stopped).await?,
            }
        } else {
            return Ok(());
        };
        let plan = Plan::from_request(&request, &observed);
        if plan.is_blocked() {
            self.store
                .set_status_for_operation(
                    &latest.id,
                    ApplicationState::Degraded,
                    Some(BlockedPlan::of(&plan).message()),
                )
                .await?;
            return Ok(());
        }
        if prepared.is_none() && !application.delete_intent() {
            return Ok(());
        }
        if !plan_requires_execution(&plan)
            && !application.delete_intent()
            && !Self::failure_outlives_converged_plan(latest)
        {
            self.store
                .set_status_for_operation(&latest.id, ApplicationState::Ready, None)
                .await?;
        }
        // Permanent failures are reconsidered only when fresh planning shows
        // their blocker has disappeared. Transient failures respect backoff.
        let was_blocked = matches!(
            Self::failure(latest),
            Some(
                DiagnosticCode::OwnershipConflict
                    | DiagnosticCode::DockerConfigurationConflict
                    | DiagnosticCode::ServiceUpdateFailed
                    | DiagnosticCode::PlanBlocked
            )
        );
        if (latest.state == OperationState::Succeeded && plan_requires_execution(&plan))
            || was_blocked
        {
            if running {
                return Box::pin(self.run_operation(latest, cancellation)).await;
            }
            if let Some(operation) = self
                .store
                .request_reconcile(application.id(), &latest.id)
                .await?
            {
                Box::pin(self.run_operation(&operation, cancellation)).await?;
            }
        }
        Ok(())
    }

    /// Runs `maintain_active`, recording non-journal failures as scan diagnostics so
    /// a failed repair never prevents the latest operation from executing.
    async fn repair_before_execution(
        &self,
        application: &StoredEnvironment,
        operation_id: &str,
        failures: &ScanFailures,
    ) -> Result<(), StoreError> {
        if let Err(error) = self.maintain_active(application.id(), operation_id).await {
            if let OperationError::Journal(error) = error {
                return Err(error);
            }
            self.record_scan_diagnostic(application.id(), &error.diagnostic(), failures)
                .await?;
            tracing::warn!(%error, "active target repair failed");
        }
        Ok(())
    }

    /// The prepared target the unpromoted operation may still roll out, if
    /// any, as `observed` shows the runtime. A failed job or a cancellation
    /// ends the deployment once no job container can still be running, and
    /// so does a failure whose own plan a conflict blocks: only an operator
    /// resolves that, and reconsidering the deployment then rolls its
    /// prerequisites out again. Unless an exclusive job of it succeeded: the
    /// services it stopped would run against data in the new format, so they
    /// stay stopped until the deployment continues. Other failures are
    /// retried, or reconsidered as soon as their plan is unblocked, so
    /// restoring the active target meanwhile would only churn their
    /// prerequisites. Independently, the Docker adapter refuses to start a
    /// service while a job holds one of its volumes, including runs of
    /// superseded operations.
    pub(super) async fn rolling_out(
        &self,
        operation: &Operation,
        observed: &piqueld_core::ObservedApplication,
    ) -> Result<Option<piqueld_core::ResolvedApplication>, StoreError> {
        let Some(prepared) = self.store.prepared_target(&operation.id).await? else {
            return Ok(None);
        };
        let ended = operation.state == OperationState::Cancelled
            || Self::job_failed(operation)
            || (operation.state == OperationState::Failed
                && self.conflict_blocks(&prepared, observed).await?
                && !self.migrated_exclusively(operation, &prepared).await?);
        let stopping = ended
            && !self
                .store
                .pending_job_cleanup(&operation.environment_id)
                .await?
                .is_empty();
        Ok((!ended || stopping).then_some(prepared))
    }

    /// Whether an exclusive job of `operation`, which prepared `target`,
    /// succeeded, so the volumes it held are in their new format.
    async fn migrated_exclusively(
        &self,
        operation: &Operation,
        target: &piqueld_core::ResolvedApplication,
    ) -> Result<bool, StoreError> {
        let succeeded = self.store.succeeded_jobs(&operation.id).await?;
        Ok(target.jobs.iter().any(|job| {
            job.exclusive_volumes().next().is_some()
                && succeeded.contains(job.logical_name.as_str())
        }))
    }

    /// Whether a conflict blocks planning `target` against `observed`: one
    /// only an operator resolves, unlike a failed service update, which
    /// restoring the active target would clear.
    async fn conflict_blocks(
        &self,
        target: &piqueld_core::ResolvedApplication,
        observed: &piqueld_core::ObservedApplication,
    ) -> Result<bool, StoreError> {
        let stopped = self.store.is_stopped(&target.id).await?;
        let desired = self.runtime_target(target, &[], observed, stopped).await?;
        let plan = Plan::from_request(&PlanRequest::Reconcile { desired }, observed);
        Ok(
            plan.is_blocked()
                && !matches!(BlockedPlan::of(&plan), BlockedPlan::ServiceUpdateFailed),
        )
    }

    /// Records a diagnostic at most once per environment and code within a pass.
    /// A health job and reconciliation can observe the same failure concurrently;
    /// the lock is held across the write so only one of them records it.
    async fn record_scan_diagnostic(
        &self,
        application: &EnvironmentId,
        diagnostic: &piqueld_core::observability::Diagnostic,
        failures: &ScanFailures,
    ) -> Result<(), StoreError> {
        let key = (application.clone(), diagnostic.code.clone());
        let mut recorded = failures.lock().await;
        if recorded.contains(&key) {
            return Ok(());
        }
        self.store
            .record_diagnostic(
                diagnostic,
                None,
                Some(application),
                crate::store::Attribution::default(),
            )
            .await?;
        recorded.insert(key);
        Ok(())
    }

    /// Keeps the currently active (published) target healthy while the latest
    /// operation has not been promoted.
    ///
    /// Applies at most `Plan::next_repair` per call (see `repair_once`),
    /// except in a stopped environment, whose services scale down without
    /// waiting, so it applies repairs until one would have to wait. Skips
    /// deletions and promoted operations.
    async fn maintain_active(
        &self,
        id: &EnvironmentId,
        operation_id: &str,
    ) -> Result<(), OperationError> {
        let operation = self.store.operation(operation_id).await?;
        if operation.kind == OperationKind::Delete || self.store.is_promoted(operation_id).await? {
            return Ok(());
        }
        let app = self.store.get(id).await?;
        let Some(target) = app.resolved else {
            return Ok(());
        };
        // Each repair changes one resource, so the target bounds them.
        let bound = target.services.len() + target.networks.len() + 1;
        for _ in 0..bound {
            if !self.repair_once(id, operation_id, &target).await?
                || !self.store.is_stopped(id).await?
            {
                break;
            }
        }
        Ok(())
    }

    /// Applies `Plan::next_repair` toward `target`, the active target, under
    /// the global mutation lock and in its own journal entry, and returns
    /// whether it did. Skips blocked plans, operations no longer the latest
    /// or promoted since, environments stopped or started since, and
    /// removals that would drop resources still
    /// referenced by routes awaiting cutover. While the deployment may still
    /// roll out, prepared job prerequisites and services stopped for an
    /// exclusive job are left to it; once it has ended, repair restores them.
    async fn repair_once(
        &self,
        id: &EnvironmentId,
        operation_id: &str,
        target: &piqueld_core::ResolvedApplication,
    ) -> Result<bool, OperationError> {
        let accepted_routes = self.store.applied_routes(id).await?;
        let observed = self.docker.observe(id).await?;
        let stopped = self.store.is_stopped(id).await?;
        let desired = self
            .runtime_target(target, &accepted_routes, &observed, stopped)
            .await?;
        let mut plan = Plan::from_request(&PlanRequest::Reconcile { desired }, &observed);
        if plan.is_blocked() {
            return Ok(false);
        }
        let _guard = self.mutations.lock().await;
        if self
            .store
            .latest_operation_for_environment(id)
            .await?
            .is_none_or(|op| op.id != operation_id)
            || self.store.is_promoted(operation_id).await?
            || self.store.is_stopped(id).await? != stopped
        {
            return Ok(false);
        }
        // Reread under the lock: a retry may have reopened a failed operation.
        if let Some(prepared) = self
            .rolling_out(&self.store.operation(operation_id).await?, &observed)
            .await?
        {
            plan.preserve_job_prerequisites(&prepared, &observed);
        }
        let Some(action) = plan.next_repair() else {
            return Ok(false);
        };
        if matches!(
            action.kind,
            piqueld_core::ActionKind::RemoveService { .. }
                | piqueld_core::ActionKind::RemoveNetwork { .. }
        ) && target.routes != accepted_routes
        {
            return Ok(false);
        }
        let ownership = self.ownership_labels(id);
        let journal = self
            .store
            .begin_action(
                Some(operation_id),
                action.kind.name(),
                Some(action.kind.resource_name()),
            )
            .await?;
        let result = match self
            .service_secrets(action.kind.secrets(), &ownership)
            .await
        {
            Ok(secrets) => match self.store.action_request(&journal, 1).await {
                Ok(()) => self
                    .mutate_action(&action.kind, &ownership, &secrets)
                    .await
                    .map_err(OperationError::from),
                Err(error) => Err(error.into()),
            },
            Err(error) => Err(error),
        };
        self.close_action(&journal, result).await.map(|()| true)
    }

    /// Prunes daemon events, the audit trail, receipts, expired auth state,
    /// build logs, and local snapshots no preview uses, plus finished
    /// operations and events older than their retention windows. A
    /// zero-day window disables pruning.
    async fn prune_history(&self, operation_days: u64, event_days: u64) -> Result<(), StoreError> {
        self.store.prune_daemon_events().await?;
        self.store.prune_audit().await?;
        self.store.prune_receipts().await?;
        self.store.prune_auth().await?;
        self.store.prune_build_logs().await?;
        self.store.prune_snapshots().await?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let cutoff = |days| {
            i64::try_from(now.saturating_sub(u128::from(days) * 86_400_000)).unwrap_or(i64::MAX)
        };
        if operation_days > 0 {
            self.store
                .prune_finished_operations(cutoff(operation_days))
                .await?;
        }
        if event_days > 0 {
            self.store.prune_events(cutoff(event_days)).await?;
        }
        Ok(())
    }
}

/// Whether the plan has work beyond retaining volumes.
pub(super) fn plan_requires_execution(plan: &Plan) -> bool {
    plan.actions
        .iter()
        .any(|action| !matches!(action.kind, piqueld_core::ActionKind::RetainVolume { .. }))
}
