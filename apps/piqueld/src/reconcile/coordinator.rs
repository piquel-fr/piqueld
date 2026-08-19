use super::{
    ActionKind, ApplicationState, Arc, CancellationToken, DockerApi, Duration, MAX_PAGE_SIZE,
    Notify, OperationScheduler, PlanAction, PlanRequest, ReconcileHandler, SchedulerError,
    SqliteStore, StoreError, StoredApplication, plan,
};

/// Runs startup recovery, apply/delete wakes, and periodic full scans. Multiple
/// wakes collapse through `Notify`, keeping polling work bounded.
///
/// # Errors
/// Returns only when cancellation is requested; transient scheduler/store failures
/// are logged and retried so the controller remains alive.
pub async fn run_coordinator<D: DockerApi>(
    scheduler: Arc<OperationScheduler<ReconcileHandler<D>>>,
    store: Arc<SqliteStore>,
    docker: Arc<D>,
    wake: Arc<Notify>,
    interval: Duration,
    cancellation: CancellationToken,
) -> Result<(), SchedulerError> {
    loop {
        match scheduler.recover_and_run(cancellation.child_token()).await {
            Ok(_) => break,
            Err(error) => {
                tracing::error!(%error, "coordinator startup recovery failed; retrying");
                tokio::select! {
                    () = cancellation.cancelled() => return Ok(()),
                    () = tokio::time::sleep(Duration::from_secs(1)) => {},
                }
            }
        }
    }
    run_scan_until_success(&scheduler, &store, &docker, &cancellation).await?;
    let mut scan = tokio::time::interval(interval);
    scan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            () = wake.notified() => run_scan_until_success(&scheduler, &store, &docker, &cancellation).await?,
            _ = scan.tick() => run_scan_until_success(&scheduler, &store, &docker, &cancellation).await?,
        }
    }
}

async fn run_scan_until_success<D: DockerApi>(
    scheduler: &OperationScheduler<ReconcileHandler<D>>,
    store: &SqliteStore,
    docker: &D,
    cancellation: &CancellationToken,
) -> Result<(), SchedulerError> {
    loop {
        match scan_and_run(scheduler, store, docker, cancellation).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                tracing::error!(%error, "coordinator scan failed; retrying");
                tokio::select! {
                    () = cancellation.cancelled() => return Ok(()),
                    () = tokio::time::sleep(Duration::from_secs(1)) => {},
                }
            }
        }
    }
}

async fn scan_and_run<D: DockerApi>(
    scheduler: &OperationScheduler<ReconcileHandler<D>>,
    store: &SqliteStore,
    docker: &D,
    cancellation: &CancellationToken,
) -> Result<(), SchedulerError> {
    scheduler.run_until_idle(cancellation.child_token()).await?;
    let mut cursor = None;
    loop {
        let page = store.list(cursor.as_deref(), MAX_PAGE_SIZE).await?;
        for application in page.items {
            if cancellation.is_cancelled() {
                return Ok(());
            }
            if application.delete_intent {
                retry_delete(store, docker, &application).await?;
                continue;
            }
            if store
                .active_reconcile(&application.application.id, application.generation)
                .await?
                .is_some()
            {
                continue;
            }
            let observed = match docker.observe(&application.application.id).await {
                Ok(observed) => observed,
                Err(error) => {
                    tracing::warn!(
                        application_id = %application.application.id,
                        %error,
                        "runtime observation failed during drift scan"
                    );
                    continue;
                }
            };
            let runtime_plan = plan(
                &PlanRequest::Reconcile {
                    desired: application.resolved.clone(),
                },
                &observed,
            );
            let status = store.status(&application.application.id).await?;
            if runtime_plan.is_blocked() {
                if status.state == ApplicationState::Ready {
                    store
                        .set_status(
                            &application.application.id,
                            ApplicationState::Ready,
                            ApplicationState::Degraded,
                            Some(application.generation),
                            Some("runtime reconciliation is blocked by an ownership conflict"),
                        )
                        .await?;
                }
                continue;
            }
            if !plan_requires_execution(&runtime_plan) {
                if status.state == ApplicationState::Degraded {
                    store
                        .set_status(
                            &application.application.id,
                            ApplicationState::Degraded,
                            ApplicationState::Ready,
                            Some(application.generation),
                            Some("runtime converged"),
                        )
                        .await?;
                }
                continue;
            }
            let steps = runtime_plan
                .actions
                .iter()
                .map(PlanAction::operation_step)
                .collect::<Vec<_>>();
            match store
                .request_reconcile(&application.application.id, application.generation, &steps)
                .await
            {
                Ok(_)
                | Err(StoreError::IllegalTransition | StoreError::GenerationConflict { .. }) => {}
                Err(error) => return Err(error.into()),
            }
        }
        let Some(next_cursor) = page.next_cursor else {
            break;
        };
        cursor = Some(next_cursor);
        if cancellation.is_cancelled() {
            return Ok(());
        }
    }
    scheduler.run_until_idle(cancellation.child_token()).await
}

async fn retry_delete<D: DockerApi>(
    store: &SqliteStore,
    docker: &D,
    application: &StoredApplication,
) -> Result<(), SchedulerError> {
    let resolved = &application.resolved;
    let observed = match docker.observe(&application.application.id).await {
        Ok(observed) => observed,
        Err(error) => {
            tracing::warn!(
                application_id = %application.application.id,
                %error,
                "runtime observation failed during delete retry scan"
            );
            return Ok(());
        }
    };
    let deletion_plan = plan(
        &PlanRequest::Delete {
            application_id: application.application.id.clone(),
            instance_id: resolved.instance_id.clone(),
        },
        &observed,
    );
    if deletion_plan.is_blocked() {
        return Ok(());
    }
    let steps = deletion_plan
        .actions
        .iter()
        .map(PlanAction::operation_step)
        .collect::<Vec<_>>();
    match store
        .request_delete(&application.application.id, application.generation, &steps)
        .await
    {
        Ok(_) | Err(StoreError::IllegalTransition | StoreError::GenerationConflict { .. }) => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub(super) fn plan_requires_execution(plan: &piqueld_core::Plan) -> bool {
    plan.actions
        .iter()
        .any(|action| !matches!(action.kind, ActionKind::RetainVolume { .. }))
}
