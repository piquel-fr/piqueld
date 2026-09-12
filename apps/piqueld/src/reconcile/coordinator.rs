use super::{
    ApplicationState, Arc, CancellationToken, Controller, DockerApi, Duration, MAX_PAGE_SIZE,
    Notify, OperationState, Plan, PlanRequest, StoreError, StoredApplication, blocked_plan_message,
};
use futures_util::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use std::collections::HashMap;

type Discovered = (bool, Vec<(StoredApplication, String)>);

impl<D: DockerApi> Controller<D> {
    /// One event loop polls application futures and discovery concurrently. No
    /// application holds the loop while waiting for Docker, SQLite, or a timer.
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
        let mut discovery: Option<BoxFuture<'_, Result<Discovered, StoreError>>> = None;
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
                let recover = !recovered;
                discovery = Some(
                    async move {
                        if recover {
                            self.store.recover_interrupted().await?;
                        }
                        if full {
                            self.prune_history(finished_operation_days, event_days)
                                .await?;
                        }
                        self.discover(full)
                            .await
                            .map(|applications| (full, applications))
                    }
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
                    match result {
                        Ok((full,applications))=> {
                            recovered=true;
                            for (application,operation_id) in applications {
                                let id=application.application.id.clone();
                                if full && health_active.insert(id.clone()) {
                                    let health_id=id.clone();
                                    let health_operation=operation_id.clone();
                                    health_jobs.push(async move {
                                        let result=async {
                                            self.maintain_active(&health_id,&health_operation).await?;
                                            let observed=self.docker.observe(&health_id).await.map_err(super::OperationError::from)?;
                                            self.store.record_health(&health_operation,&observed).await.map_err(super::OperationError::from)
                                        }.await;
                                        (health_id,result)
                                    });
                                }

                                if let Some((current,token))=active.get(&id) {
                                    let token: &CancellationToken=token;
                                    if current!=&operation_id { token.cancel(); }
                                    continue;
                                }
                                let token=cancellation.child_token();
                                active.insert(id.clone(),(operation_id,token.clone()));
                                jobs.push(async move {
                                    let result=self.scan_application(&application,&token).await;
                                    (id,result)
                                });
                            }
                        }
                        Err(error)=>tracing::error!(%error,"application discovery failed"),
                    }
                }
                Some((id,result))=health_jobs.next(), if !health_jobs.is_empty()=> {
                    health_active.remove(&id);
                    if let Err(error)=result { tracing::warn!(application_id=%id,%error,"health reporting failed"); }
                }
                Some((id,result))=jobs.next(), if !jobs.is_empty()=> {
                    active.remove(&id);
                    if let Err(error)=result { tracing::warn!(application_id=%id,%error,"application reconciliation failed"); }
                    requested=true;
                }
            }
        }
    }

    async fn discover(&self, full: bool) -> Result<Vec<(StoredApplication, String)>, StoreError> {
        let mut cursor = None;
        let mut applications = Vec::new();
        loop {
            let page = self.store.list(cursor.as_deref(), MAX_PAGE_SIZE).await?;
            for app in page.items {
                if let Some(operation) = self
                    .store
                    .latest_operation_for_application(&app.application.id)
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
        for (application, _) in applications {
            jobs.push(async move { self.scan_application(&application, cancellation).await });
        }
        while let Some(result) = jobs.next().await {
            result?;
        }
        Ok(())
    }

    fn retry_due(operation: &super::Operation) -> bool {
        let Some(code) = operation.error_code.as_deref() else {
            return false;
        };
        if !matches!(
            code,
            "docker_unavailable"
                | "image_resolution_failed"
                | "docker_request_failed"
                | "convergence_timeout"
                | "journal_unavailable"
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

    async fn scan_application(
        &self,
        application: &StoredApplication,
        cancellation: &CancellationToken,
    ) -> Result<(), StoreError> {
        let Some(latest) = self
            .store
            .latest_operation_for_application(&application.application.id)
            .await?
        else {
            return Ok(());
        };
        if let Err(error) = self
            .maintain_active(&application.application.id, &latest.id)
            .await
        {
            tracing::warn!(%error,"active target repair failed");
        }
        if latest.state == OperationState::Requested
            || (latest.state == OperationState::Running && latest.error_code.is_none())
        {
            return self.run_operation(&latest, cancellation).await;
        }
        if Self::retry_due(&latest) {
            let operation = if latest.state == OperationState::Running {
                latest
            } else {
                self.store.retry_operation(&latest).await?
            };
            return self.run_operation(&operation, cancellation).await;
        }
        let application = self.store.get(&application.application.id).await?;
        let observed = match self.docker.observe(&application.application.id).await {
            Ok(observed) => observed,
            Err(error) => {
                tracing::warn!(%error,"could not observe application health");
                return Ok(());
            }
        };
        // An older resolved target is useful for reporting health, but cannot
        // authorize corrective work once newer intent has been accepted.
        let prepared = self.store.prepared_target(&latest.id).await?;
        let target = prepared.as_ref().or(application.resolved.as_ref());
        let request = if application.delete_intent {
            PlanRequest::Delete {
                application_id: application.application.id.clone(),
                instance_id: piqueld_core::InstanceId::parse(self.store.instance_id())
                    .expect("valid store identity"),
            }
        } else if let Some(target) = target {
            PlanRequest::Reconcile {
                desired: target.clone(),
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
        if !plan_requires_execution(&plan) && !application.delete_intent {
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
                return self.run_operation(&latest, cancellation).await;
            }
            if let Some(operation) = self
                .store
                .request_reconcile(&application.application.id, &latest.id)
                .await?
            {
                self.run_operation(&operation, cancellation).await?;
            }
        }
        Ok(())
    }

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
        let observed = self.docker.observe(id).await?;
        let plan = Plan::from_request(
            &PlanRequest::Reconcile {
                desired: target.clone(),
            },
            &observed,
        );
        if plan.is_blocked() {
            return Ok(());
        }
        let Some(action) = plan
            .actions
            .iter()
            .find(|action| action.kind.mutates_runtime())
        else {
            return Ok(());
        };
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
        let result = self
            .mutate_action(&action.kind, &ownership)
            .await
            .map_err(super::OperationError::from);
        let error = result
            .as_ref()
            .err()
            .map(|error| (error.code(), error.message()));
        self.store
            .maintenance_event(
                operation_id,
                action.kind.name(),
                action.kind.resource_name(),
                error
                    .as_ref()
                    .map(|(code, message)| (*code, message.as_str())),
            )
            .await?;
        result
    }

    async fn prune_history(&self, operation_days: u64, event_days: u64) -> Result<(), StoreError> {
        self.store.prune_receipts().await?;
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

pub(super) fn plan_requires_execution(plan: &Plan) -> bool {
    plan.actions
        .iter()
        .any(|action| !matches!(action.kind, piqueld_core::ActionKind::RetainVolume { .. }))
}
