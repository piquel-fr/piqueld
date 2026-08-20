use super::{
    ActionKind, ApplicationState, Arc, CancellationToken, DockerApi, Duration, MAX_PAGE_SIZE,
    Notify, OperationScheduler, Plan, PlanAction, PlanRequest, ReconcileHandler, RetryPolicy,
    SchedulerError, SqliteStore, StoreError, StoredApplication, blocked_plan_message,
};
use crate::store::ApplicationStatus;
use piqueld_core::resource::ObservedApplication;

/// Runs startup recovery, performs an initial scan, and continues scanning after wake notifications or at regular intervals.
///
/// Cancellation causes the coordinator to exit successfully. Recovery and scan failures are retried according to the coordinator's retry behavior.
///
/// # Errors
///
/// Returns a [`SchedulerError`] if a scan cannot be completed.
///
/// # Examples
///
/// ```no_run
/// # async fn example<D: DockerApi>(
/// #     scheduler: Arc<OperationScheduler<ReconcileHandler<D>>>,
/// #     store: Arc<SqliteStore>,
/// #     docker: Arc<D>,
/// #     wake: Arc<Notify>,
/// # ) -> Result<(), SchedulerError> {
/// let cancellation = CancellationToken::new();
/// run_coordinator(
///     scheduler,
///     store,
///     docker,
///     wake,
///     Duration::from_secs(60),
///     cancellation,
/// ).await
/// # }
/// ```
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

/// Repeatedly runs the coordinator scan until it succeeds or cancellation is requested.
///
/// Failed scans are retried with exponentially increasing delays up to the retry policy's maximum delay.
///
/// # Examples
///
/// ```rust,ignore
/// run_scan_until_success(&scheduler, &store, &docker, &cancellation)
///     .await
///     .expect("scan should complete successfully");
/// ```
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

/// Drains pending operations and scans all stored applications until processing completes or cancellation is requested.
///
/// Cancellation causes the scan to finish successfully without processing remaining applications.
///
/// # Examples
///
/// ```ignore
/// scan_and_run(&scheduler, &store, &docker, &cancellation).await?;
/// ```
///
/// # Returns
///
/// `Ok(())` when processing completes or cancellation is requested; otherwise, the encountered scheduler or store error.
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

/// Processes an application by retrying deletion or reconciling its state.
///
/// # Examples
///
/// ```no_run
/// # async fn example<D: DockerApi>(
/// #     store: &SqliteStore,
/// #     docker: &D,
/// #     application: &StoredApplication,
/// # ) -> Result<(), StoreError> {
/// scan_application(store, docker, application).await?;
/// # Ok(())
/// # }
/// ```
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

/// Reconciles an application with its desired state and records the resulting status or schedules required operations.
///
/// Applications with blocked plans are marked as degraded. Applications that require no execution are marked as recovered;
/// otherwise, the required reconciliation steps are submitted to the store.
///
/// # Examples
///
/// ```rust,ignore
/// scan_reconcile_application(&store, &docker, &application).await?;
/// # Ok::<(), StoreError>(())
/// ```
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

/// Loads the runtime observation and persisted status for an application eligible for reconciliation.
///
/// Returns no context when reconciliation is already active or runtime observation fails. Store
/// errors are propagated to the caller.
///
/// # Examples
///
/// ```ignore
/// if let Some((observed, status)) = load_reconcile_context(&store, &docker, &application).await? {
///     // Use the observed runtime state and persisted status for reconciliation.
/// }
/// # Ok::<(), StoreError>(())
/// ```
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

/// Marks a ready application as degraded when its plan is blocked.
///
/// Applications in other states remain unchanged.
///
/// # Examples
///
/// ```ignore
/// record_blocked_status(
///     &store,
///     &application,
///     &plan,
///     ApplicationState::Ready,
/// )
/// .await?;
/// # Ok::<(), StoreError>(())
/// ```
///
/// # Errors
///
/// Returns a [`StoreError`] if updating the application status fails.
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

/// Records recovery when an application in a degraded or failed state has converged.
///
/// Applications in other states are left unchanged.
///
/// # Examples
///
/// ```no_run
/// record_recovered_status(&store, &application, ApplicationState::Degraded).await?;
/// # Ok::<(), StoreError>(())
/// ```
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

/// Retries deletion reconciliation for a stored application.
///
/// Runtime observation failures and blocked deletion plans are logged and skipped.
/// Illegal state transitions and generation conflicts are also ignored; other store
/// errors are returned.
///
/// # Examples
///
/// ```no_run
/// retry_delete(&store, &docker, &application).await?;
/// # Ok::<(), StoreError>(())
/// ```
///
/// `store` persists the deletion request, `docker` provides the observed runtime
/// state, and `application` identifies the application and its resolved instance.
///
/// # Errors
///
/// Returns a [`StoreError`] when persisting the deletion request fails for a
/// reason other than an illegal transition or generation conflict.
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

/// Determines whether a plan contains an action that requires execution.
///
/// # Examples
///
/// ```
/// let plan = piqueld_core::Plan::default();
/// assert!(!plan_requires_execution(&plan));
/// ```
///
/// Returns `true` when the plan contains an action other than retaining a volume,
/// and `false` otherwise.
pub(super) fn plan_requires_execution(plan: &piqueld_core::Plan) -> bool {
    plan.actions
        .iter()
        .any(|action| !matches!(action.kind, ActionKind::RetainVolume { .. }))
}
