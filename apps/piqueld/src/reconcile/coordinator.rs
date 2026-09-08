use super::{
    ApplicationState, Arc, CancellationToken, Controller, DockerApi, Duration, MAX_PAGE_SIZE,
    Notify, OperationState, Plan, PlanRequest, StoreError, StoredApplication, blocked_plan_message,
};

impl<D: DockerApi> Controller<D> {
    /// Reconciles applications sequentially on API wakes and periodic drift scans.
    /// Interrupted operations are planned again from current Docker state on startup.
    ///
    /// # Errors
    /// Store failures are logged and retried on the next scan.
    pub async fn run(
        &self,
        wake: Arc<Notify>,
        interval: Duration,
        finished_operation_days: u64,
        cancellation: CancellationToken,
    ) -> Result<(), StoreError> {
        let mut recovered = false;
        let mut scan = tokio::time::interval(interval);
        scan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = cancellation.cancelled() => return Ok(()),
                () = wake.notified() => {},
                _ = scan.tick() => {},
            }
            if !recovered {
                match self.store.recover_interrupted().await {
                    Ok(_) => recovered = true,
                    Err(error) => {
                        tracing::error!(%error, "could not recover interrupted operations");
                        continue;
                    }
                }
            }
            tokio::select! {
                () = cancellation.cancelled() => return Ok(()),
                result = self.scan(&cancellation) => {
                    if let Err(error) = result {
                        tracing::error!(%error, "application scan failed");
                    }
                },
            }
            self.prune_finished_operations(finished_operation_days)
                .await;
        }
    }

    /// Runs one reconciliation pass over current applications.
    ///
    /// # Errors
    /// Returns a store error if applications cannot be listed. Individual
    /// application failures are recorded or logged before the pass continues.
    pub async fn scan(&self, cancellation: &CancellationToken) -> Result<(), StoreError> {
        let mut cursor = None;
        loop {
            let page = self.store.list(cursor.as_deref(), MAX_PAGE_SIZE).await?;
            for application in page.items {
                if cancellation.is_cancelled() {
                    return Ok(());
                }
                if let Err(error) = self.scan_application(&application, cancellation).await {
                    tracing::warn!(application_id = %application.application.id, %error,
                        "application reconciliation failed");
                }
            }
            let Some(next_cursor) = page.next_cursor else {
                return Ok(());
            };
            cursor = Some(next_cursor);
        }
    }

    async fn scan_application(
        &self,
        application: &StoredApplication,
        cancellation: &CancellationToken,
    ) -> Result<(), StoreError> {
        let latest = self
            .store
            .latest_operation_for_application(&application.application.id)
            .await?;
        if let Some(operation) = &latest
            && matches!(
                operation.state,
                OperationState::Requested | OperationState::Running
            )
        {
            return self.run_operation(operation, cancellation).await;
        }
        let Some(latest) = latest else {
            return Ok(());
        };
        // Load desired state after its operation ID; compare-and-set writes below
        // reject this observation if an apply arrives while Docker is inspected.
        let application = self.store.get(&application.application.id).await?;
        if application.delete_intent {
            return Ok(());
        }

        let observed = match self.docker.observe(&application.application.id).await {
            Ok(observed) => observed,
            Err(error) => {
                tracing::warn!(application_id = %application.application.id, %error,
                    "could not observe application drift");
                return Ok(());
            }
        };
        let plan = Plan::from_request(
            &PlanRequest::Reconcile {
                desired: application.resolved.clone(),
            },
            &observed,
        );
        let status = self.store.status(&application.application.id).await?;
        if plan.is_blocked() {
            if status.state == ApplicationState::Ready {
                self.store
                    .set_status_for_operation(
                        &latest.id,
                        ApplicationState::Degraded,
                        Some(blocked_plan_message(&plan)),
                    )
                    .await?;
            }
        } else if !plan_requires_execution(&plan) {
            if matches!(
                status.state,
                ApplicationState::Failed | ApplicationState::Degraded
            ) {
                self.store
                    .set_status_for_operation(
                        &latest.id,
                        ApplicationState::Ready,
                        Some("runtime converged"),
                    )
                    .await?;
            }
        } else if latest.state == OperationState::Succeeded
            && let Some(operation) = self
                .store
                .request_reconcile(&application.application.id, &latest.id)
                .await?
        {
            self.run_operation(&operation, cancellation).await?;
        }
        Ok(())
    }

    async fn prune_finished_operations(&self, days: u64) {
        if days == 0 {
            return;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let cutoff = now.saturating_sub(u128::from(days) * 86_400_000);
        let cutoff = i64::try_from(cutoff).unwrap_or(i64::MAX);
        if let Err(error) = self.store.prune_finished_operations(cutoff).await {
            tracing::error!(%error, "could not prune finished operations");
        }
    }
}

pub(super) fn plan_requires_execution(plan: &Plan) -> bool {
    plan.actions
        .iter()
        .any(|action| !matches!(action.kind, piqueld_core::ActionKind::RetainVolume { .. }))
}
