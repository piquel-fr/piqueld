use super::{
    ActionKind, ApplicationState, Arc, CancellationToken, DockerApi, Duration, MAX_PAGE_SIZE,
    Notify, OperationScheduler, Plan, PlanAction, PlanRequest, ReconcileHandler, RetryPolicy,
    SchedulerError, SqliteStore, StoreError, StoredApplication, blocked_plan_message,
};
use crate::store::ApplicationStatus;
use piqueld_core::resource::ObservedApplication;

/// Operations stuck in `running` longer than this lease are reclaimed back to
/// `recovery` so one transient journal failure cannot freeze an application
/// until restart. The lease comfortably exceeds prepare plus convergence plus
/// retry budgets.
const RUNNING_LEASE_MS: i64 = 30 * 60 * 1000;

/// Milliseconds in one day; converts the configured finished-operation
/// retention into a pruning cutoff.
const MILLISECONDS_PER_DAY: i64 = 24 * 60 * 60 * 1000;

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
    finished_operation_days: u64,
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
        reclaim_stale_running(&store).await;
        prune_finished_operations(&store, finished_operation_days).await;
        tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            () = wake.notified() => run_scan_until_success(&scheduler, &store, &docker, &cancellation).await?,
            _ = scan.tick() => run_scan_until_success(&scheduler, &store, &docker, &cancellation).await?,
        }
    }
}

/// Deletes terminal operations beyond the configured retention. A retention of
/// zero disables pruning entirely.
async fn prune_finished_operations(store: &SqliteStore, finished_operation_days: u64) {
    if finished_operation_days == 0 {
        return;
    }
    let cutoff_ms = now_minus_days(finished_operation_days);
    match store.prune_finished_operations(cutoff_ms).await {
        Ok(counts) if counts.operations + counts.idempotency_keys > 0 => {
            tracing::debug!(
                operations = counts.operations,
                idempotency_keys = counts.idempotency_keys,
                "pruned finished operations beyond their retention"
            );
        }
        Ok(_) => {}
        Err(error) => {
            tracing::error!(%error, "could not prune finished operations");
        }
    }
}

fn now_minus_days(days: u64) -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or_default();
    let cutoff = now.saturating_sub(
        u128::from(days)
            * u128::try_from(MILLISECONDS_PER_DAY).expect("milliseconds per day fits in u128"),
    );
    i64::try_from(cutoff).unwrap_or(i64::MAX)
}

/// Returns operations whose journal writes failed mid-execution to the
/// recoverable queue. Handlers are idempotent and per-application locks
/// serialize re-execution, so reclamation is safe.
async fn reclaim_stale_running(store: &SqliteStore) {
    match store.reclaim_expired_running(RUNNING_LEASE_MS).await {
        Ok(0) => {}
        Ok(reclaimed) => {
            tracing::warn!(
                reclaimed,
                "reclaimed operations stuck in running beyond their lease"
            );
        }
        Err(error) => {
            tracing::error!(%error, "could not reclaim stale running operations");
        }
    }
}

async fn run_scan_until_success<D: DockerApi>(
    scheduler: &OperationScheduler<ReconcileHandler<D>>,
    store: &SqliteStore,
    docker: &D,
    cancellation: &CancellationToken,
) -> Result<(), SchedulerError> {
    let retry = RetryPolicy::default();
    let mut delay = retry.initial_delay;
    let mut logged_failure = false;
    loop {
        match scan_and_run(scheduler, store, docker, cancellation).await {
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
) -> Result<(), SchedulerError> {
    scheduler.run_until_idle(cancellation.child_token()).await?;
    let mut cursor = None;
    loop {
        let page = store.list(cursor.as_deref(), MAX_PAGE_SIZE).await?;
        for application in page.items {
            if cancellation.is_cancelled() {
                return Ok(());
            }
            if let Err(error) = scan_application(store, docker, &application).await {
                tracing::warn!(
                    application_id = %application.application.id,
                    %error,
                    "application scan failed; continuing with the remaining applications"
                );
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
    let runtime_plan = Plan::from_request(
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
    let deletion_plan = Plan::from_request(
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
