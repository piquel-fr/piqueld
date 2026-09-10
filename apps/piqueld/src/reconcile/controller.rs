use super::{
    ApplicationState, CancellationToken, Controller, DockerApi, Operation, OperationError,
    OperationKind, OperationState, Plan, PlanRequest, StoreError, blocked_plan_error,
};
use crate::application::RuntimeBoundary;
use std::sync::Arc;

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
        if operation.state == OperationState::Running && operation.error_code.is_some() {
            self.store
                .transition_operation(
                    &operation.id,
                    OperationState::Running,
                    OperationState::Requested,
                    None,
                )
                .await?;
            self.store
                .transition_operation(
                    &operation.id,
                    OperationState::Requested,
                    OperationState::Running,
                    None,
                )
                .await?;
        }
        let operation = &self.store.operation(&operation.id).await?;
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
        self.store
            .progress(&operation.id, "preparing", None)
            .await
            .map_err(OperationError::from)?;
        let application = self
            .store
            .get(&operation.application_id)
            .await
            .map_err(OperationError::from)?;
        let request = if operation.kind == OperationKind::Delete {
            PlanRequest::Delete {
                application_id: operation.application_id.clone(),
                instance_id: piqueld_core::InstanceId::parse(self.store.instance_id())
                    .expect("valid store identity"),
            }
        } else {
            PlanRequest::Reconcile {
                desired: tokio::select! {
                    ()=cancellation.cancelled()=>return Err(OperationError::Cancelled),
                    result=self.prepare_target(operation,&application)=>result?,
                },
            }
        };
        let ownership = self.ownership_labels(&application.application.id);
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
            let observed = tokio::time::timeout_at(
                deadline,
                self.observe_with_retry(operation, cancellation, deadline),
            )
            .await
            .map_err(|_| OperationError::ConvergenceTimeout)??;
            self.check_current(operation).await?;
            self.store
                .record_health(&operation.id, &observed)
                .await
                .map_err(OperationError::from)?;
            let plan = Plan::from_request(&request, &observed);
            self.check_plan(operation, &plan).await?;
            if operation.kind != OperationKind::Delete {
                let _guard = self.mutations.lock().await;
                self.check_current(operation).await?;
                self.store
                    .publish_prepared(operation)
                    .await
                    .map_err(OperationError::from)?;
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

    async fn check_plan(&self, operation: &Operation, plan: &Plan) -> Result<(), OperationError> {
        let resource = plan
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.blocking)
            .map(|diagnostic| diagnostic.resource.as_str());
        self.store
            .progress(&operation.id, "planning", resource)
            .await
            .map_err(OperationError::from)?;
        if plan.is_blocked() {
            return Err(blocked_plan_error(plan));
        }
        Ok(())
    }

    async fn prepare_target(
        &self,
        operation: &Operation,
        application: &super::StoredApplication,
    ) -> Result<piqueld_core::ResolvedApplication, OperationError> {
        self.check_current(operation).await?;
        if let Some(target) = self
            .store
            .prepared_target(&operation.id)
            .await
            .map_err(OperationError::from)?
        {
            return Ok(target);
        }
        let runtime = crate::application::DockerRuntime::new(
            Arc::clone(&self.docker),
            piqueld_core::InstanceId::parse(self.store.instance_id()).expect("valid identity"),
            Arc::new(tokio::sync::Notify::new()),
            self.prepare_timeout,
        )
        .with_progress(Arc::clone(&self.store), operation.id.clone());
        let reusable = if operation.kind == OperationKind::Refresh {
            piqueld_core::ResolutionSet::default()
        } else {
            application
                .resolved
                .as_ref()
                .map_or_else(piqueld_core::ResolutionSet::default, |target| {
                    target.reusable_resolutions(&application.application)
                })
        };
        let prepared = runtime
            .prepare(&application.application, &reusable)
            .await
            .map_err(|error| match error {
                crate::application::BoundaryError::Store(error) => OperationError::from(error),
                crate::application::BoundaryError::Runtime(error) => OperationError::from(error),
                crate::application::BoundaryError::Compilation(errors) => {
                    tracing::error!(?errors, "application compilation failed");
                    OperationError::ValidationFailed("compile application")
                }
            })?;
        self.check_current(operation).await?;
        self.store
            .save_prepared(operation, &prepared)
            .await
            .map_err(OperationError::from)?;
        Ok(prepared)
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
