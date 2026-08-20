use super::blocked_plan_error;
use super::coordinator::plan_requires_execution;
use super::{
    ApplicationState, CancellationToken, DockerApi, Operation, OperationError, OperationHandler,
    OperationKind, Plan, PlanRequest, ReconcileHandler, StepState, StoredApplication,
};
use async_trait::async_trait;

#[async_trait]
impl<D: DockerApi> OperationHandler for ReconcileHandler<D> {
    /// Executes an operation and records eligible failures.
    ///
    /// Cancellation and supersession errors are returned without recording. Errors encountered while recording a failure are logged, while the original operation result is preserved.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(
    /// #     handler: &ReconcileHandler<D>,
    /// #     operation: &Operation,
    /// #     cancellation: &CancellationToken,
    /// # ) where D: DockerApi {
    /// let result = handler.execute(operation, cancellation).await;
    /// # }
    /// ```
    async fn execute(
        &self,
        operation: &Operation,
        cancellation: &CancellationToken,
    ) -> Result<(), OperationError> {
        let result = self.execute_operation(operation, cancellation).await;
        if let Err(error) = result
            && !matches!(
                error,
                OperationError::Cancelled | OperationError::Superseded
            )
            && !cancellation.is_cancelled()
            && let Err(recording_error) = self.record_operation_failure(operation, error).await
        {
            tracing::error!(
                %recording_error,
                operation_id = %operation.id,
                "could not record operation failure"
            );
        }
        result
    }
}

impl<D: DockerApi> ReconcileHandler<D> {
    /// Executes an operation against the application's current state, updating its status and validating convergence.
    ///
    /// Superseded operations skip their remaining steps without mutating application state. Delete operations
    /// complete only after the application is no longer present in the runtime state.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// handler.execute_operation(&operation, &cancellation).await?;
    /// # Ok::<(), OperationError>(())
    /// ```
    ///
    /// # Returns
    ///
    /// `Ok(())` when the operation completes or is safely superseded; an `OperationError` when execution,
    /// state updates, or convergence validation fails.
    async fn execute_operation(
        &self,
        operation: &Operation,
        cancellation: &CancellationToken,
    ) -> Result<(), OperationError> {
        let Some(application) = self.load_application(operation).await? else {
            return Ok(());
        };
        if operation.kind != OperationKind::Delete && application.generation != operation.generation
        {
            // A newer generation owns the application now. The newer operation will
            // re-plan against the current runtime; this operation must not mutate it.
            self.skip_superseded_steps(operation).await?;
            return Ok(());
        }

        self.start_deployment(operation).await?;
        let request = request_for(operation, &application);
        let ownership = Self::ownership_labels(&application);
        let steps = self
            .store
            .operation_steps(&operation.id)
            .await
            .map_err(journal_error)?;
        self.execute_steps(operation, &request, steps, &ownership, cancellation)
            .await?;
        if operation.kind != OperationKind::Delete && !self.operation_is_current(operation).await? {
            self.skip_superseded_steps(operation).await?;
            return Ok(());
        }
        self.mark_ready(operation).await?;
        if operation.kind == OperationKind::Delete {
            return self.finish_delete(operation, &request, cancellation).await;
        }
        Ok(())
    }
}

/// Builds the plan request corresponding to an operation and its stored application state.
///
/// Delete operations use the stored instance identifier; other operations use the
/// resolved desired state.
///
/// # Examples
///
/// ```
/// # let operation: Operation = todo!();
/// # let app: StoredApplication = todo!();
/// let request = request_for(&operation, &app);
/// assert!(matches!(
///     request,
///     PlanRequest::Delete { .. } | PlanRequest::Reconcile { .. }
/// ));
/// ```
fn request_for(operation: &Operation, app: &StoredApplication) -> PlanRequest {
    if operation.kind == OperationKind::Delete {
        PlanRequest::Delete {
            application_id: operation.application_id.clone(),
            instance_id: app.resolved.instance_id.clone(),
        }
    } else {
        PlanRequest::Reconcile {
            desired: app.resolved.clone(),
        }
    }
}
/// Converts a store failure into an operation error indicating that the journal is unavailable.
///
/// # Examples
///
/// ```ignore
/// let operation_error = journal_error(store_error);
/// assert!(matches!(operation_error, OperationError::JournalUnavailable));
/// ```
рминистр
pub(super) fn journal_error(error: crate::store::StoreError) -> OperationError {
    tracing::error!(error = ?error, "operation journal request failed");
    drop(error);
    OperationError::JournalUnavailable
}

impl<D: DockerApi> ReconcileHandler<D> {
    /// Loads the stored application associated with an operation.
    ///
    /// Missing applications are treated as successfully finalized for delete operations.
    /// Other store failures are reported as unavailable state.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let application = handler.load_application(&operation).await?;
    /// if let Some(application) = application {
    ///     // Use the stored application state.
    /// }
    /// # Ok::<(), OperationError>(())
    /// ```
    async fn load_application(
        &self,
        operation: &Operation,
    ) -> Result<Option<StoredApplication>, OperationError> {
        match self.store.get(&operation.application_id).await {
            Ok(application) => Ok(Some(application)),
            Err(crate::store::StoreError::NotFound) if operation.kind == OperationKind::Delete => {
                // Finalization tombstones the application before the scheduler marks the
                // operation successful. A crash in that tiny window resumes here safely.
                Ok(None)
            }
            Err(error) => {
                tracing::error!(error = ?error, "application state request failed");
                Err(OperationError::StateUnavailable)
            }
        }
    }

    /// Transitions a pending non-delete application to the deploying state.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// handler.start_deployment(&operation).await?;
    /// assert_eq!(handler.store.status(&operation.application_id).await?.state, ApplicationState::Deploying);
    /// ```
    ///
    /// # Returns
    ///
    /// `Ok(())` when the deployment status is started or no transition is required.
    /// Returns an error if the application status cannot be read or updated.
    async fn start_deployment(&self, operation: &Operation) -> Result<(), OperationError> {
        let status = self
            .store
            .status(&operation.application_id)
            .await
            .map_err(journal_error)?;
        if operation.kind != OperationKind::Delete && status.state == ApplicationState::Pending {
            self.store
                .set_status(
                    &operation.application_id,
                    ApplicationState::Pending,
                    ApplicationState::Deploying,
                    None,
                    None,
                )
                .await
                .map_err(journal_error)?;
        }
        Ok(())
    }

    /// Executes each pending operation step in order, stopping when the operation is cancelled or superseded.
    
    ///
    
    /// Completed and skipped steps are ignored. Non-delete operations from superseded generations
    
    /// skip their remaining steps without executing them.
    
    ///
    
    /// # Examples
    
    ///
    
    /// ```rust,ignore
    
    /// handler
    
    ///     .execute_steps(&operation, &request, steps, &ownership, &cancellation)
    
    ///     .await?;
    
    /// ```
    async fn execute_steps(
        &self,
        operation: &Operation,
        request: &PlanRequest,
        steps: Vec<crate::store::OperationStep>,
        ownership: &std::collections::BTreeMap<String, String>,
        cancellation: &CancellationToken,
    ) -> Result<(), OperationError> {
        for step in steps {
            if matches!(step.state, StepState::Succeeded | StepState::Skipped) {
                continue;
            }
            if operation.kind != OperationKind::Delete
                && !self.operation_is_current(operation).await?
            {
                self.skip_superseded_steps(operation).await?;
                return Ok(());
            }
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            self.execute_step(operation, request, &step, ownership, cancellation)
                .await?;
        }
        Ok(())
    }

    /// Executes a reconciliation step against the currently observed application state.
    ///
    /// The step is skipped when its action is no longer present in the current plan.
    /// Otherwise, it runs the action and records the resulting step state, including
    /// failures and cancellation-aware recovery.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(handler: &Handler, operation: &Operation, request: &PlanRequest,
    /// #     step: &OperationStep, ownership: &BTreeMap<String, String>,
    /// #     cancellation: &CancellationToken) -> Result<(), OperationError> {
    /// handler.execute_step(operation, request, step, ownership, cancellation).await?;
    /// # Ok(())
    /// # }
    /// ```
    async fn execute_step(
        &self,
        operation: &Operation,
        request: &PlanRequest,
        step: &crate::store::OperationStep,
        ownership: &std::collections::BTreeMap<String, String>,
        cancellation: &CancellationToken,
    ) -> Result<(), OperationError> {
        let deadline = tokio::time::Instant::now() + self.retry.convergence_timeout;
        let observed = self
            .observe_with_retry(&operation.application_id, cancellation, deadline)
            .await?;
        let current = Plan::from_request(request, &observed);
        if current.is_blocked() {
            return Err(blocked_plan_error(&current));
        }
        let Some(action) = current
            .actions
            .iter()
            .find(|action| action.operation_step() == step.action)
            .cloned()
        else {
            self.store
                .transition_step(&step.id, step.state, StepState::Skipped, None)
                .await
                .map_err(journal_error)?;
            return Ok(());
        };
        self.store
            .transition_step(&step.id, step.state, StepState::Running, None)
            .await
            .map_err(journal_error)?;
        match self
            .execute_action(&action, &operation.application_id, ownership, cancellation)
            .await
        {
            Ok(()) => {
                self.store
                    .transition_step(&step.id, StepState::Running, StepState::Succeeded, None)
                    .await
                    .map_err(journal_error)?;
                Ok(())
            }
            Err(error) => {
                self.record_step_failure(step, error, cancellation).await?;
                Err(error)
            }
        }
    }

    /// Records a failed step as recovery when cancellation is active, or as failed otherwise.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// // A cancelled operation leaves the step eligible for recovery.
    /// handler.record_step_failure(&step, error, &cancellation).await?;
    /// # Ok::<(), OperationError>(())
    /// ```
    ///
    /// # Arguments
    ///
    /// * `step` - The operation step whose state should be updated.
    /// * `error` - The error encountered while executing the step.
    /// * `cancellation` - The operation's cancellation token.
    ///
    /// # Returns
    ///
    /// `Ok(())` when the step state is recorded successfully; otherwise, the journal error.
    async fn record_step_failure(
        &self,
        step: &crate::store::OperationStep,
        error: OperationError,
        cancellation: &CancellationToken,
    ) -> Result<(), OperationError> {
        if cancellation.is_cancelled() {
            self.store
                .transition_step(&step.id, StepState::Running, StepState::Recovery, None)
                .await
                .map_err(journal_error)?;
            return Ok(());
        }
        self.store
            .transition_step(&step.id, StepState::Running, StepState::Failed, Some(error))
            .await
            .map_err(journal_error)?;
        Ok(())
    }

    /// Records an operation failure by transitioning the application to an appropriate failed or degraded state.
    ///
    /// Superseded non-delete operations are ignored. The current application status determines whether a
    /// failure status transition is applicable.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// handler
    ///     .record_operation_failure(&operation, error)
    ///     .await
    ///     .expect("failed to record operation failure");
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if the current application status cannot be read or the failure status cannot be recorded.
    pub(super) async fn record_operation_failure
    pub(super) async fn record_operation_failure(
        &self,
        operation: &Operation,
        error: OperationError,
    ) -> Result<(), OperationError> {
        if operation.kind != OperationKind::Delete && !self.operation_is_current(operation).await? {
            return Ok(());
        }
        let message = error.message();
        let status = self
            .store
            .status(&operation.application_id)
            .await
            .map_err(journal_error)?;
        match (operation.kind, status.state) {
            (OperationKind::Delete, ApplicationState::Deleting) => {
                self.store
                    .set_status(
                        &operation.application_id,
                        ApplicationState::Deleting,
                        ApplicationState::Failed,
                        Some(operation.generation),
                        Some(&message),
                    )
                    .await
                    .map_err(journal_error)?;
            }
            (_, ApplicationState::Deploying) if operation.kind != OperationKind::Delete => {
                self.store
                    .set_status(
                        &operation.application_id,
                        ApplicationState::Deploying,
                        ApplicationState::Degraded,
                        Some(operation.generation),
                        Some(&message),
                    )
                    .await
                    .map_err(journal_error)?;
            }
            (_, ApplicationState::Ready | ApplicationState::Degraded)
                if operation.kind != OperationKind::Delete =>
            {
                self.store
                    .set_status(
                        &operation.application_id,
                        status.state,
                        ApplicationState::Degraded,
                        Some(operation.generation),
                        Some(&message),
                    )
                    .await
                    .map_err(journal_error)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Marks pending and recovery steps as skipped when an operation has been superseded.
    ///
    /// Steps that are running, failed, or cancelled cause the operation to remain
    /// superseded so the scheduler can cancel it. Completed steps are left unchanged.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// let result = handler.skip_superseded_steps(&operation).await;
    /// assert!(result.is_ok() || matches!(result, Err(OperationError::Superseded)));
    /// ```
    async fn skip_superseded_steps(&self, operation: &Operation) -> Result<(), OperationError> {
        let steps = self
            .store
            .operation_steps(&operation.id)
            .await
            .map_err(journal_error)?;
        let mut requires_cancellation = false;
        for step in steps {
            match step.state {
                StepState::Pending | StepState::Recovery => {
                    self.store
                        .transition_step(&step.id, step.state, StepState::Skipped, None)
                        .await
                        .map_err(journal_error)?;
                }
                StepState::Running | StepState::Failed | StepState::Cancelled => {
                    // Preserve terminal failure details. The scheduler cancels the
                    // superseded parent instead of trying to report it as successful.
                    requires_cancellation = true;
                }
                StepState::Succeeded | StepState::Skipped => {}
            }
        }
        if requires_cancellation {
            Err(OperationError::Superseded)
        } else {
            Ok(())
        }
    }

    /// Marks a converged non-delete application as ready.
    ///
    /// Delete operations are left unchanged, and applications that are already ready
    /// are not updated.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// handler.mark_ready(&operation).await?;
    /// # Ok::<(), OperationError>(())
    /// ```
    ///
    /// # Returns
    ///
    /// `Ok(())` when the application is ready or the operation is a delete;
    /// otherwise, the journal error encountered while reading or updating status.
    async fn mark_ready(&self, operation: &Operation) -> Result<(), OperationError> {
        if operation.kind == OperationKind::Delete {
            return Ok(());
        }
        let status = self
            .store
            .status(&operation.application_id)
            .await
            .map_err(journal_error)?;
        if status.state != ApplicationState::Ready {
            self.store
                .set_status(
                    &operation.application_id,
                    status.state,
                    ApplicationState::Ready,
                    Some(operation.generation),
                    Some("runtime converged"),
                )
                .await
                .map_err(journal_error)?;
        }
        Ok(())
    }

    /// Verifies that a deletion has converged after re-observing the application.
    ///
    /// # Errors
    ///
    /// Returns an error if the deletion remains blocked or requires further execution.
    /// Propagates errors encountered while observing the application.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// handler.finish_delete(&operation, &request, &cancellation).await?;
    /// ```
    async fn finish_delete(
        &self,
        operation: &Operation,
        request: &PlanRequest,
        cancellation: &CancellationToken,
    ) -> Result<(), OperationError> {
        let deadline = tokio::time::Instant::now() + self.retry.convergence_timeout;
        let observed = self
            .observe_with_retry(&operation.application_id, cancellation, deadline)
            .await?;
        let current = Plan::from_request(request, &observed);
        let error = if current.is_blocked() {
            blocked_plan_error(&current)
        } else if plan_requires_execution(&current) {
            OperationError::DeletionNotConverged
        } else {
            return Ok(());
        };
        Err(error)
    }

    /// Determines whether an operation targets the application's current generation.
    ///
    /// # Examples
    ///
    /// ```
    /// let is_current = handler.operation_is_current(&operation).await?;
    /// assert!(is_current);
    /// ```
    ///
    /// # Returns
    ///
    /// `Ok(true)` when the operation generation matches the stored application generation; `Ok(false)` otherwise. Store failures are returned as `OperationError`.
    async fn operation_is_current(&self, operation: &Operation) -> Result<bool, OperationError> {
        let application = self
            .store
            .get(&operation.application_id)
            .await
            .map_err(journal_error)?;
        Ok(application.generation == operation.generation)
    }
}
