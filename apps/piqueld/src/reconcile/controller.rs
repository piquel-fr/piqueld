use super::{
    ApplicationState, CancellationToken, Controller, DockerApi, Operation, OperationError,
    OperationKind, OperationState, Plan, PlanRequest, StoreError, blocked_plan_error,
};

impl<D: DockerApi> Controller<D> {
    pub(super) async fn run_operation(
        &self,
        operation: &Operation,
        cancellation: &CancellationToken,
    ) -> Result<(), StoreError> {
        if operation.state == OperationState::Requested {
            match self
                .store
                .transition_operation(
                    &operation.id,
                    OperationState::Requested,
                    OperationState::Running,
                    None,
                )
                .await
            {
                Ok(()) => {}
                Err(StoreError::IllegalTransition) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        let result = self.execute_operation(operation, cancellation).await;
        if cancellation.is_cancelled() {
            return Ok(());
        }
        match result {
            Ok(()) if operation.kind == OperationKind::Delete => {
                self.store.finish_delete_operation(operation).await
            }
            Ok(()) => {
                self.store
                    .transition_operation(
                        &operation.id,
                        OperationState::Running,
                        OperationState::Succeeded,
                        None,
                    )
                    .await
            }
            Err(OperationError::Cancelled | OperationError::Superseded) => {
                // A request may already have cancelled the operation while a Docker
                // call was in flight. Its durable state takes precedence.
                match self
                    .store
                    .transition_operation(
                        &operation.id,
                        OperationState::Running,
                        OperationState::Cancelled,
                        None,
                    )
                    .await
                {
                    Ok(()) | Err(StoreError::IllegalTransition) => Ok(()),
                    Err(error) => Err(error),
                }
            }
            Err(error) if operation.kind == OperationKind::Delete => {
                self.store
                    .record_operation_error(operation, error.code(), &error.message())
                    .await
            }
            Err(error) => {
                self.record_failure(operation, error).await?;
                self.store
                    .transition_operation(
                        &operation.id,
                        OperationState::Running,
                        OperationState::Failed,
                        Some((error.code(), &error.message())),
                    )
                    .await
            }
        }
    }

    /// Plans from fresh observations until no work remains. Only desired state and
    /// operation status are durable; Docker state determines the next action.
    async fn execute_operation(
        &self,
        operation: &Operation,
        cancellation: &CancellationToken,
    ) -> Result<(), OperationError> {
        let application = self
            .store
            .get(&operation.application_id)
            .await
            .map_err(OperationError::from)?;
        let request = if operation.kind == OperationKind::Delete {
            PlanRequest::Delete {
                application_id: operation.application_id.clone(),
                instance_id: application.resolved.instance_id.clone(),
            }
        } else {
            PlanRequest::Reconcile {
                desired: application.resolved.clone(),
            }
        };
        let ownership = Self::ownership_labels(&application);
        if operation.kind != OperationKind::Delete
            && !self
                .store
                .set_status_for_operation(&operation.id, ApplicationState::Deploying, None)
                .await
                .map_err(OperationError::from)?
        {
            return Err(OperationError::Superseded);
        }
        let deadline = tokio::time::Instant::now() + self.retry.convergence_timeout;
        loop {
            self.check_current(operation).await?;
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(OperationError::ConvergenceTimeout);
            }
            let observed = self
                .observe_with_retry(operation, cancellation, deadline)
                .await?;
            self.check_current(operation).await?;
            let plan = Plan::from_request(&request, &observed);
            if plan.is_blocked() {
                return Err(blocked_plan_error(&plan));
            }
            let action = plan.actions.iter().find(|action| {
                !matches!(action.kind, piqueld_core::ActionKind::RetainVolume { .. })
            });
            let Some(action) = action else {
                if operation.kind != OperationKind::Delete
                    && !self
                        .store
                        .set_status_for_operation(&operation.id, ApplicationState::Ready, None)
                        .await
                        .map_err(OperationError::from)?
                {
                    return Err(OperationError::Superseded);
                }
                return Ok(());
            };
            tokio::time::timeout_at(
                deadline,
                self.execute_action(action, operation, &ownership, cancellation),
            )
            .await
            .map_err(|_| OperationError::ConvergenceTimeout)??;
        }
    }

    pub(super) async fn check_current(&self, operation: &Operation) -> Result<(), OperationError> {
        let current = self
            .store
            .latest_operation_for_application(&operation.application_id)
            .await
            .map_err(OperationError::from)?;
        let Some(current) = current.filter(|current| current.id == operation.id) else {
            return Err(OperationError::Superseded);
        };
        if current.state != OperationState::Running {
            return Err(OperationError::Cancelled);
        }
        Ok(())
    }

    async fn record_failure(
        &self,
        operation: &Operation,
        error: OperationError,
    ) -> Result<(), StoreError> {
        self.store
            .set_status_for_operation(
                &operation.id,
                ApplicationState::Degraded,
                Some(&error.message()),
            )
            .await?;
        Ok(())
    }
}
