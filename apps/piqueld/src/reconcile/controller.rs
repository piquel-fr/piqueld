use super::{
    ApplicationState, CancellationToken, Controller, DockerApi, Operation, OperationError,
    OperationKind, OperationState, Plan, PlanRequest, StoreError, blocked_plan_error,
};
use crate::application::RuntimeBoundary;
use std::sync::Arc;

impl<D: DockerApi> Controller<D> {
    #[tracing::instrument(skip_all, fields(
        application_id = %operation.application_id,
        operation_id = %operation.id,
        generation = operation.generation,
        operation_kind = ?operation.kind,
    ))]
    pub(super) async fn run_operation(
        &self,
        operation: &Operation,
        cancellation: &CancellationToken,
    ) -> Result<(), StoreError> {
        let started = std::time::Instant::now();
        tracing::info!("operation started");
        let result = self.run_operation_inner(operation, cancellation).await;
        let duration_ms = started.elapsed().as_secs_f64() * 1_000.0;
        match &result {
            Ok(outcome) => tracing::info!(outcome, duration_ms, "operation execution completed"),
            Err(error) => tracing::error!(?error, duration_ms, "operation journal update failed"),
        }
        result.map(|_| ())
    }

    async fn run_operation_inner(
        &self,
        operation: &Operation,
        cancellation: &CancellationToken,
    ) -> Result<&'static str, StoreError> {
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
                Err(StoreError::IllegalTransition) => return Ok("superseded"),
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
        let result = Box::pin(self.execute_operation(operation, cancellation)).await;
        if cancellation.is_cancelled() {
            return Ok("cancelled");
        }
        let outcome = match &result {
            Ok(()) => "succeeded",
            Err(error) => error.code(),
        };
        let persisted = match result {
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
                // A request may already have superseded the operation while a Docker
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
                tracing::warn!(code = error.code(), error = ?error, "deletion will be retried");
                self.store
                    .record_operation_error(operation, error.code(), &error.message())
                    .await
            }
            Err(error) => {
                tracing::error!(code = error.code(), error = ?error, "operation failed");
                self.record_failure(operation, &error).await?;
                self.store
                    .transition_operation(
                        &operation.id,
                        OperationState::Running,
                        OperationState::Failed,
                        Some((error.code(), &error.message())),
                    )
                    .await
            }
        };
        persisted.map(|()| outcome)
    }

    /// Plans from fresh observations until no work remains. Only desired state and
    /// operation status are durable; Docker state determines the next action.
    #[tracing::instrument(skip_all, fields(phase = "convergence"))]
    async fn execute_operation(
        &self,
        operation: &Operation,
        cancellation: &CancellationToken,
    ) -> Result<(), OperationError> {
        let request = self.operation_request(operation, cancellation).await?;
        let deadline = tokio::time::Instant::now() + self.retry.convergence_timeout;
        if operation.kind == OperationKind::Delete {
            self.withdraw_routes(operation, deadline).await?;
        }
        let ownership = self.ownership_labels(&operation.application_id);
        if operation.kind != OperationKind::Delete
            && !self
                .store
                .set_status_for_operation(&operation.id, ApplicationState::Deploying, None)
                .await
                .map_err(OperationError::from)?
        {
            return Err(OperationError::Superseded);
        }
        tracing::debug!(
            timeout_seconds = self.retry.convergence_timeout.as_secs(),
            "convergence started"
        );
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
            let accepted_routes = self.store.applied_routes(&operation.application_id).await?;
            let runtime_request = match &request {
                PlanRequest::Reconcile { desired } => PlanRequest::Reconcile {
                    desired: desired
                        .clone()
                        .with_ingress_routes(self.ingress_enabled(), &accepted_routes),
                },
                _ => request.clone(),
            };
            let plan = Plan::from_request(&runtime_request, &observed);
            self.check_plan(operation, &plan).await?;
            if operation.kind != OperationKind::Delete {
                let _guard = self.mutations.lock().await;
                self.check_current(operation).await?;
                self.store
                    .publish_prepared(operation)
                    .await
                    .map_err(OperationError::from)?;
                if let PlanRequest::Reconcile { desired } = &request {
                    tokio::time::timeout_at(
                        deadline,
                        self.sync_routes(
                            operation,
                            &desired.routes,
                            plan.desired_resources_ready(),
                        ),
                    )
                    .await
                    .map_err(|_| OperationError::ConvergenceTimeout)??;
                }
            }
            if self.store.applied_routes(&operation.application_id).await? != accepted_routes {
                // Replan after cutover before dropping old ingress attachments.
                continue;
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

    /// Public exposure must be withdrawn within the deletion's convergence budget.
    async fn withdraw_routes(
        &self,
        operation: &Operation,
        deadline: tokio::time::Instant,
    ) -> Result<(), OperationError> {
        tokio::time::timeout_at(deadline, async {
            let _guard = self.mutations.lock().await;
            self.check_current(operation).await?;
            self.sync_routes(operation, &[], true).await
        })
        .await
        .map_err(|_| OperationError::ConvergenceTimeout)?
    }

    async fn operation_request(
        &self,
        operation: &Operation,
        cancellation: &CancellationToken,
    ) -> Result<PlanRequest, OperationError> {
        self.store
            .progress(&operation.id, "preparing", None)
            .await
            .map_err(OperationError::from)?;
        let application = self
            .store
            .get(&operation.application_id)
            .await
            .map_err(OperationError::from)?;
        Ok(if operation.kind == OperationKind::Delete {
            PlanRequest::Delete {
                application_id: operation.application_id.clone(),
                instance_id: piqueld_core::InstanceId::parse(self.store.instance_id())
                    .expect("valid store identity"),
            }
        } else {
            PlanRequest::Reconcile {
                desired: tokio::select! {
                    ()=cancellation.cancelled()=>return Err(OperationError::Cancelled),
                    result=tokio::time::timeout(self.prepare_timeout, self.prepare_target(operation,&application))=>result.map_err(|_| OperationError::ValidationFailed("preparation timed out"))??,
                },
            }
        })
    }

    async fn sync_routes(
        &self,
        operation: &Operation,
        routes: &[piqueld_core::manifest::ValidatedRoute],
        ready: bool,
    ) -> Result<(), OperationError> {
        if routes.is_empty() && !self.store.has_routes(&operation.application_id).await? {
            return Ok(());
        }
        self.store.progress(&operation.id, "routing", None).await?;
        if let Some(ingress) = &self.ingress {
            Box::pin(ingress.apply(operation, routes, ready))
                .await
                .map_err(OperationError::Ingress)?;
        } else {
            self.store
                .stage_routes(
                    &operation.application_id,
                    routes,
                    ready,
                    Some(&operation.id),
                )
                .await?;
            let table = self.store.routing_table().await?;
            self.store.acknowledge_routes(&table).await?;
        }
        Ok(())
    }

    async fn check_plan(&self, operation: &Operation, plan: &Plan) -> Result<(), OperationError> {
        tracing::debug!(
            actions = plan.actions.len(),
            blocked = plan.is_blocked(),
            "observation planned"
        );
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

    #[tracing::instrument(skip_all, fields(phase = "preparation", timeout_seconds = self.prepare_timeout.as_secs()))]
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
        let runtime = crate::application::ApplicationRuntime::new(
            Arc::clone(&self.docker),
            piqueld_core::InstanceId::parse(self.store.instance_id()).expect("valid identity"),
            Arc::new(tokio::sync::Notify::new()),
            self.prepare_timeout,
        )
        .with_progress(Arc::clone(&self.store), operation.id.clone());
        let snapshot = self.store.deployment_manifest(&operation.id).await?;
        let manifest = self.deployment_manifest(operation, &snapshot).await?;
        // A rename changes display metadata without rewriting deployment history.
        let manifest = manifest.with_name(application.application.metadata().name.clone());
        let reusable = if operation.kind == OperationKind::Refresh {
            piqueld_core::ResolutionSet::default()
        } else {
            application
                .resolved
                .as_ref()
                .map_or_else(piqueld_core::ResolutionSet::default, |target| {
                    target.reusable_resolutions(&manifest)
                })
        };
        let prepared =
            runtime
                .prepare(&manifest, &reusable)
                .await
                .map_err(|error| match error {
                    crate::application::BoundaryError::Store(error) => OperationError::from(error),
                    crate::application::BoundaryError::Runtime(error) => {
                        OperationError::from(error)
                    }
                    crate::application::BoundaryError::GitBuild(error) => {
                        tracing::warn!(error = ?error, "Git source build failed");
                        OperationError::GitBuildFailed(error)
                    }
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
        error: &OperationError,
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
