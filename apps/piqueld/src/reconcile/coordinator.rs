use super::{
    ApplicationState, Arc, CancellationToken, Controller, DockerApi, Duration, MAX_PAGE_SIZE,
    Notify, OperationState, Plan, PlanRequest, StoreError, StoredApplication, blocked_plan_message,
};
use futures_util::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use std::collections::{HashMap, HashSet};
use tracing::Instrument;

/// Result of a discovery pass: whether it was a full scan, and each selected
/// application with the ID of its latest operation.
type Discovered = (bool, Vec<(StoredApplication, String)>);
/// Application/diagnostic-code pairs already recorded during one discovery pass.
type ScanFailures = Arc<tokio::sync::Mutex<HashSet<(piqueld_core::ApplicationId, String)>>>;

impl<D: DockerApi> Controller<D> {
    /// One event loop polls application futures and discovery concurrently. No
    /// application holds the loop while waiting for Docker, `SQLite`, or a timer.
    /// Failures are recorded inside each future, never in the loop body: a job
    /// suspended while holding the writer or a failure set must keep being polled.
    /// Process startup closes abandoned actions before any worker starts; this
    /// loop runs beside ingress, whose daemon actions it must not interrupt.
    /// # Panics
    /// Panics if the scan interval is zero.
    /// # Errors
    /// Store failures are logged and retried; shutdown cancels pending local work.
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
        let mut health_active = std::collections::HashSet::new();
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
            tokio::select! {
                ()=cancellation.cancelled()=>return Ok(()),
                ()=wake.notified()=>requested=true,
                _=tick.tick()=>requested=true,
                result=async { match discovery.as_mut() {Some(discovery)=>discovery.await,None=>std::future::pending().await} }=> {
                    discovery=None;
                    if let Some((full,applications))=result {
                            recovered=true;
                            let failures = ScanFailures::default();
                            for (application,operation_id) in applications {
                                let id=application.application.id().clone();
                                // Full scans refresh health, with at most one job per application.
                                if full && health_active.insert(id.clone()) {
                                    let health_id=id.clone();
                                    let health_operation=operation_id.clone();
                                    let health_failures=Arc::clone(&failures);
                                    let span = tracing::debug_span!("application_health", application_id = %id, operation_id = %operation_id, generation = application.generation);
                                    health_jobs.push(async move {
                                        let result=async {
                                            self.maintain_active(&health_id,&health_operation).await?;
                                            let observed=self.docker.observe(&health_id).await.map_err(super::OperationError::from)?;
                                            self.store.record_health(&health_operation,&observed).await.map_err(super::OperationError::from)
                                        }.await;
                                        if let Err(error)=result {
                                            if let Err(report_error)=self.record_scan_diagnostic(&health_id,&error.diagnostic(),&health_failures).await {
                                                tracing::error!(application_id=%health_id,error=?report_error,"health diagnostic could not be persisted");
                                            }
                                            tracing::warn!(application_id=%health_id,operation_id=%health_operation,generation=application.generation,%error,"health reporting failed");
                                        }
                                        health_id
                                    }.instrument(span));
                                }

                                // A running job is cancelled if a newer operation replaced its own;
                                // its completion requests another pass that picks up the new one.
                                if let Some((current,token))=active.get(&id) {
                                    let token: &CancellationToken=token;
                                    if current!=&operation_id { token.cancel(); }
                                    continue;
                                }
                                let token=cancellation.child_token();
                                active.insert(id.clone(),(operation_id,token.clone()));
                                let scan_failures=Arc::clone(&failures);
                                jobs.push(async move {
                                    if let Err(error)=Box::pin(self.scan_application(&application,&token,&scan_failures)).await {
                                        let failure=super::OperationError::Journal(error);
                                        self.store.report_diagnostic(&failure.diagnostic(),Some(&id)).await;
                                    }
                                    id
                                });
                            }
                    }
                }
                Some(id)=health_jobs.next(), if !health_jobs.is_empty()=> {
                    health_active.remove(&id);
                }
                Some(id)=jobs.next(), if !jobs.is_empty()=> {
                    active.remove(&id);
                    requested=true;
                }
            }
        }
    }

    /// Recovers interrupted work on the first pass, prunes history on full
    /// scans, and finds applications to process. Failures are reported here.
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
                let failure = super::OperationError::Journal(error);
                self.store
                    .report_diagnostic(&failure.diagnostic(), None)
                    .await;
                None
            }
        }
    }

    /// Pages through all applications and selects those needing work. A full scan
    /// selects every application with an operation; otherwise only requested,
    /// cleanly running, or retry-due operations are selected.
    async fn discover(&self, full: bool) -> Result<Vec<(StoredApplication, String)>, StoreError> {
        let mut cursor = None;
        let mut applications = Vec::new();
        loop {
            let page = self.store.list(cursor.as_deref(), MAX_PAGE_SIZE).await?;
            for app in page.items {
                if let Some(operation) = self
                    .store
                    .latest_operation_for_application(app.application.id())
                    .await?
                    && (full
                        || operation.state == OperationState::Requested
                        || (operation.state == OperationState::Running
                            && operation.error_code.is_none())
                        || Self::retry_due(&operation))
                {
                    applications.push((app, operation.id));
                }
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(applications);
            }
        }
    }

    /// Runs one concurrent observation/reconciliation pass, also used by runtime tests.
    /// # Errors
    /// Returns a store error if applications cannot be discovered or processed.
    pub async fn scan(&self, cancellation: &CancellationToken) -> Result<(), StoreError> {
        let applications = self.discover(true).await?;
        let mut jobs = FuturesUnordered::new();
        let failures = ScanFailures::default();
        for (application, _) in applications {
            let scan_failures = Arc::clone(&failures);
            jobs.push(async move {
                Box::pin(self.scan_application(&application, cancellation, &scan_failures)).await
            });
        }
        while let Some(result) = jobs.next().await {
            result?;
        }
        Ok(())
    }

    /// Whether a transiently failed operation has waited out its backoff.
    /// Only transient error codes qualify; the delay doubles per consecutive
    /// failure from 5s and is capped at 60s.
    ///
    /// ```text
    /// failures: 1   2    3    4    5+
    /// delay:    5s  10s  20s  40s  60s
    /// ```
    fn retry_due(operation: &super::Operation) -> bool {
        let Some(code) = operation.error_code.as_deref() else {
            return false;
        };
        if !matches!(
            code,
            "ingress_unavailable"
                | "docker_unavailable"
                | "image_resolution_failed"
                | "docker_request_failed"
                | "convergence_timeout"
                | "preparation_timeout"
                | "journal_unavailable"
                | "swarm_manager_unavailable"
                | "swarm_topology_unsupported"
        ) {
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

    /// Processes one application during a scan.
    ///
    /// 1. Repairs drift in the active target while a newer target is unpromoted.
    /// 2. Runs requested, cleanly running, or retry-due operations.
    /// 3. Otherwise plans the latest target against fresh observations: blocked
    ///    plans degrade the application, converged plans mark it ready, and drift
    ///    after success (or a cleared permanent blocker) reopens the operation.
    ///
    /// Observation failures are recorded as deduplicated diagnostics, not errors.
    #[tracing::instrument(skip_all, fields(application_id = %application.application.id(), generation = application.generation))]
    async fn scan_application(
        &self,
        application: &StoredApplication,
        cancellation: &CancellationToken,
        failures: &ScanFailures,
    ) -> Result<(), StoreError> {
        let Some(latest) = self
            .store
            .latest_operation_for_application(application.application.id())
            .await?
        else {
            return Ok(());
        };
        self.repair_before_execution(application, &latest.id, failures)
            .await?;
        if latest.state == OperationState::Requested
            || (latest.state == OperationState::Running && latest.error_code.is_none())
        {
            return Box::pin(self.run_operation(&latest, cancellation)).await;
        }
        if Self::retry_due(&latest) {
            let operation = if latest.state == OperationState::Running {
                latest
            } else {
                self.store.retry_operation(&latest).await?
            };
            return Box::pin(self.run_operation(&operation, cancellation)).await;
        }
        let application = self.store.get(application.application.id()).await?;
        let observed = match self.docker.observe(application.application.id()).await {
            Ok(observed) => observed,
            Err(error) => {
                self.record_scan_diagnostic(
                    application.application.id(),
                    &super::OperationError::from(error).diagnostic(),
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
        let request = if application.delete_intent {
            PlanRequest::Delete {
                application_id: application.application.id().clone(),
                instance_id: piqueld_core::InstanceId::parse(self.store.instance_id())
                    .expect("valid store identity"),
            }
        } else if let Some(target) = target {
            PlanRequest::Reconcile {
                desired: target.clone().with_ingress(self.ingress_enabled()),
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
                    Some(blocked_plan_message(&plan)),
                )
                .await?;
            return Ok(());
        }
        if prepared.is_none() && !application.delete_intent {
            return Ok(());
        }
        if !plan_requires_execution(&plan)
            && !application.delete_intent
            && latest.error_code.as_deref() != Some("ingress_unavailable")
        {
            self.store
                .set_status_for_operation(&latest.id, ApplicationState::Ready, None)
                .await?;
        }
        // Permanent failures are reconsidered only when fresh planning shows
        // their blocker has disappeared. Transient failures respect backoff.
        let was_blocked = latest.error_code.as_deref().is_some_and(|code| {
            matches!(
                code,
                "ownership_conflict"
                    | "docker_configuration_conflict"
                    | "service_update_failed"
                    | "plan_blocked"
            )
        });
        if (latest.state == OperationState::Succeeded && plan_requires_execution(&plan))
            || was_blocked
        {
            if latest.state == OperationState::Running {
                return Box::pin(self.run_operation(&latest, cancellation)).await;
            }
            if let Some(operation) = self
                .store
                .request_reconcile(application.application.id(), &latest.id)
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
        application: &StoredApplication,
        operation_id: &str,
        failures: &ScanFailures,
    ) -> Result<(), StoreError> {
        if let Err(error) = self
            .maintain_active(application.application.id(), operation_id)
            .await
        {
            if let super::OperationError::Journal(error) = error {
                return Err(error);
            }
            self.record_scan_diagnostic(
                application.application.id(),
                &error.diagnostic(),
                failures,
            )
            .await?;
            tracing::warn!(%error,"active target repair failed");
        }
        Ok(())
    }

    /// Records a diagnostic at most once per application and code within a pass.
    /// A health job and reconciliation can observe the same failure concurrently;
    /// the lock is held across the write so only one of them records it.
    async fn record_scan_diagnostic(
        &self,
        application: &piqueld_core::ApplicationId,
        diagnostic: &piqueld_core::observability::Diagnostic,
        failures: &ScanFailures,
    ) -> Result<(), StoreError> {
        let key = (application.clone(), diagnostic.code.clone());
        let mut recorded = failures.lock().await;
        if recorded.contains(&key) {
            return Ok(());
        }
        self.store
            .record_diagnostic(diagnostic, None, Some(application))
            .await?;
        recorded.insert(key);
        Ok(())
    }

    /// Keeps the currently active (published) target healthy while the latest
    /// operation has not been promoted.
    ///
    /// Applies at most `Plan::next_repair` per call, under the global
    /// mutation lock and in its own journal entry. Skips deletions, promoted
    /// operations, blocked plans, and removals that would drop resources still
    /// referenced by routes awaiting cutover.
    async fn maintain_active(
        &self,
        id: &piqueld_core::ApplicationId,
        operation_id: &str,
    ) -> Result<(), super::OperationError> {
        let operation = self.store.operation(operation_id).await?;
        if operation.kind == super::OperationKind::Delete
            || self.store.is_promoted(operation_id).await?
        {
            return Ok(());
        }
        let app = self.store.get(id).await?;
        let Some(target) = app.resolved else {
            return Ok(());
        };
        let accepted_routes = self.store.applied_routes(id).await?;
        let observed = self.docker.observe(id).await?;
        let plan = Plan::from_request(
            &PlanRequest::Reconcile {
                desired: target
                    .clone()
                    .with_ingress_routes(self.ingress_enabled(), &accepted_routes),
            },
            &observed,
        );
        if plan.is_blocked() {
            return Ok(());
        }
        let Some(action) = plan.next_repair() else {
            return Ok(());
        };
        if matches!(
            action.kind,
            piqueld_core::ActionKind::RemoveService { .. }
                | piqueld_core::ActionKind::RemoveNetwork { .. }
        ) && target.routes != accepted_routes
        {
            return Ok(());
        }
        let _guard = self.mutations.lock().await;
        if self
            .store
            .latest_operation_for_application(id)
            .await?
            .is_none_or(|op| op.id != operation_id)
            || self.store.is_promoted(operation_id).await?
        {
            return Ok(());
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
                    .map_err(super::OperationError::from),
                Err(error) => Err(error.into()),
            },
            Err(error) => Err(error),
        };
        self.store
            .finish_action(
                &journal,
                result.as_ref().err().map(super::OperationError::diagnostic),
            )
            .await?;
        result
    }

    /// Prunes daemon events, receipts, and build logs, plus finished operations and
    /// events older than their retention windows. A zero-day window disables pruning.
    async fn prune_history(&self, operation_days: u64, event_days: u64) -> Result<(), StoreError> {
        self.store.prune_daemon_events().await?;
        self.store.prune_receipts().await?;
        self.store.prune_build_logs().await?;
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
