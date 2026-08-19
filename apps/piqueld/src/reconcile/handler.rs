use super::blocked_plan_error;
use super::coordinator::plan_requires_execution;
use super::{
    ApplicationState, CancellationToken, DockerApi, Operation, OperationError, OperationHandler,
    OperationKind, PlanRequest, ReconcileHandler, StepState, StoredApplication, plan,
};
use async_trait::async_trait;

#[async_trait]
impl<D: DockerApi> OperationHandler for ReconcileHandler<D> {
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
        {
            self.record_operation_failure(operation, error).await?;
        }
        result
    }
}

impl<D: DockerApi> ReconcileHandler<D> {
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
        let steps = self
            .store
            .operation_steps(&operation.id)
            .await
            .map_err(journal_error)?;
        self.execute_steps(operation, &request, steps, cancellation)
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
pub(super) fn journal_error(error: crate::store::StoreError) -> OperationError {
    tracing::error!(error = ?error, "operation journal request failed");
    drop(error);
    OperationError::JournalUnavailable
}

impl<D: DockerApi> ReconcileHandler<D> {
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

    async fn execute_steps(
        &self,
        operation: &Operation,
        request: &PlanRequest,
        steps: Vec<crate::store::OperationStep>,
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
            self.execute_step(operation, request, &step, cancellation)
                .await?;
        }
        Ok(())
    }

    async fn execute_step(
        &self,
        operation: &Operation,
        request: &PlanRequest,
        step: &crate::store::OperationStep,
        cancellation: &CancellationToken,
    ) -> Result<(), OperationError> {
        let deadline = tokio::time::Instant::now() + self.retry.convergence_timeout;
        let observed = self
            .observe_with_retry(&operation.application_id, cancellation, deadline)
            .await?;
        let current = plan(request, &observed);
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
            .execute_action(&action, &operation.application_id, cancellation)
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
            _ => {}
        }
        Ok(())
    }

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
        let current = plan(request, &observed);
        let error = if current.is_blocked() {
            blocked_plan_error(&current)
        } else if plan_requires_execution(&current) {
            OperationError::DeletionNotConverged
        } else {
            self.store
                .finalize_delete(&operation.application_id, operation.generation)
                .await
                .map_err(journal_error)?;
            return Ok(());
        };
        Err(error)
    }

    async fn operation_is_current(&self, operation: &Operation) -> Result<bool, OperationError> {
        let application = self
            .store
            .get(&operation.application_id)
            .await
            .map_err(journal_error)?;
        Ok(application.generation == operation.generation)
    }
}
