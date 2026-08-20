use super::{
    ActionKind, ApplicationState, Arc, CancellationToken, DockerApi, Duration, MAX_PAGE_SIZE,
    Notify, OperationScheduler, PlanAction, PlanRequest, ReconcileHandler, RetryPolicy,
    SchedulerError, SecretService, SqliteStore, StoreError, StoredApplication,
    blocked_plan_message,
};
use crate::store::ApplicationStatus;
use piqueld_core::{plan, resource::ObservedApplication};

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
    secret_service: Option<Arc<SecretService>>,
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
    run_scan_until_success(
        &scheduler,
        &store,
        &docker,
        &cancellation,
        secret_service.as_deref(),
    )
    .await?;
    let mut scan = tokio::time::interval(interval);
    scan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            () = wake.notified() => run_scan_until_success(&scheduler, &store, &docker, &cancellation, secret_service.as_deref()).await?,
            _ = scan.tick() => run_scan_until_success(&scheduler, &store, &docker, &cancellation, secret_service.as_deref()).await?,
        }
    }
}

async fn run_scan_until_success<D: DockerApi>(
    scheduler: &OperationScheduler<ReconcileHandler<D>>,
    store: &SqliteStore,
    docker: &D,
    cancellation: &CancellationToken,
    secret_service: Option<&SecretService>,
) -> Result<(), SchedulerError> {
    let retry = RetryPolicy::default();
    let mut delay = retry.initial_delay;
    let mut logged_failure = false;
    loop {
        match scan_and_run(scheduler, store, docker, cancellation, secret_service).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                if logged_failure {
                    tracing::debug!(%error, "coordinator scan still failing; retrying");
                } else {
                    tracing::warn!(%error, "coordinator scan failed; retrying");
                    logged_failure = true;
                }
                tokio::select! {
                    () = cancellation.cancelled() => return Ok(()),
                    () = tokio::time::sleep(delay) => {},
                }
                delay = delay.saturating_mul(2).min(retry.max_delay);
            }
        }
    }
}

async fn scan_and_run<D: DockerApi>(
    scheduler: &OperationScheduler<ReconcileHandler<D>>,
    store: &SqliteStore,
    docker: &D,
    cancellation: &CancellationToken,
    secret_service: Option<&SecretService>,
) -> Result<(), SchedulerError> {
    scheduler.run_until_idle(cancellation.child_token()).await?;
    let mut cursor = None;
    loop {
        let page = store.list(cursor.as_deref(), MAX_PAGE_SIZE).await?;
        for application in page.items {
            if cancellation.is_cancelled() {
                return Ok(());
            }
            if should_defer_secret_recovery(secret_service, &application).await {
                continue;
            }
            scan_application(store, docker, &application).await?;
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

async fn scan_application<D: DockerApi>(
    store: &SqliteStore,
    docker: &D,
    application: &StoredApplication,
) -> Result<(), StoreError> {
    if application.delete_intent {
        retry_delete(store, docker, application).await
    } else {
        scan_reconcile_application(store, docker, application).await
    }
}

async fn scan_reconcile_application<D: DockerApi>(
    store: &SqliteStore,
    docker: &D,
    application: &StoredApplication,
) -> Result<(), StoreError> {
    let Some((observed, status)) = load_reconcile_context(store, docker, application).await? else {
        return Ok(());
    };
    let runtime_plan = plan(
        &PlanRequest::Reconcile {
            desired: application.resolved.clone(),
        },
        &observed,
    );
    if runtime_plan.is_blocked() {
        return record_blocked_status(store, application, &runtime_plan, status.state).await;
    }
    if !plan_requires_execution(&runtime_plan) {
        return record_recovered_status(store, application, status.state).await;
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
        Ok(_) | Err(StoreError::IllegalTransition | StoreError::GenerationConflict { .. }) => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

async fn load_reconcile_context<D: DockerApi>(
    store: &SqliteStore,
    docker: &D,
    application: &StoredApplication,
) -> Result<Option<(ObservedApplication, ApplicationStatus)>, StoreError> {
    let active_reconcile = store
        .active_reconcile(&application.application.id, application.generation)
        .await?;
    if active_reconcile.is_some() {
        return Ok(None);
    }
    let observed = match docker.observe(&application.application.id).await {
        Ok(observed) => observed,
        Err(error) => {
            tracing::warn!(
                application_id = %application.application.id,
                %error,
                "runtime observation failed during drift scan"
            );
            return Ok(None);
        }
    };
    let status = store.status(&application.application.id).await?;
    Ok(Some((observed, status)))
}

async fn record_blocked_status(
    store: &SqliteStore,
    application: &StoredApplication,
    plan: &piqueld_core::Plan,
    current_state: ApplicationState,
) -> Result<(), StoreError> {
    if current_state != ApplicationState::Ready {
        return Ok(());
    }
    store
        .set_status(
            &application.application.id,
            ApplicationState::Ready,
            ApplicationState::Degraded,
            Some(application.generation),
            Some(blocked_plan_message(plan)),
        )
        .await
}

async fn record_recovered_status(
    store: &SqliteStore,
    application: &StoredApplication,
    current_state: ApplicationState,
) -> Result<(), StoreError> {
    if !matches!(
        current_state,
        ApplicationState::Degraded | ApplicationState::Failed
    ) {
        return Ok(());
    }
    store
        .set_status(
            &application.application.id,
            current_state,
            ApplicationState::Ready,
            Some(application.generation),
            Some("runtime converged"),
        )
        .await
}

async fn should_defer_secret_recovery(
    secret_service: Option<&SecretService>,
    application: &StoredApplication,
) -> bool {
    let Some(secret_service) = secret_service else {
        return false;
    };
    match secret_service.synchronize_application(application).await {
        Ok(false) => false,
        Ok(true) => {
            // The durable replacement owns the next generation. Re-read it on the
            // following scan before planning runtime work.
            true
        }
        Err(error) => {
            tracing::warn!(
                application_id = %application.application.id,
                %error,
                "secret generation recovery deferred"
            );
            true
        }
    }
}

async fn retry_delete<D: DockerApi>(
    store: &SqliteStore,
    docker: &D,
    application: &StoredApplication,
) -> Result<(), StoreError> {
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
        tracing::warn!(
            application_id = %application.application.id,
            diagnostics = ?deletion_plan.diagnostics,
            "deletion reconciliation is blocked"
        );
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
        Err(error) => return Err(error),
    }
    Ok(())
}

pub(super) fn plan_requires_execution(plan: &piqueld_core::Plan) -> bool {
    plan.actions
        .iter()
        .any(|action| !matches!(action.kind, ActionKind::RetainVolume { .. }))
}
