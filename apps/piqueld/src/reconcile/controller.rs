use super::{
    ApplicationState, CancellationToken, Controller, DockerApi, Operation, OperationError,
    OperationKind, OperationState, Plan, PlanRequest, StoreError, blocked_plan_error,
};
use crate::application::RuntimeBoundary;
use std::sync::Arc;

impl<D: DockerApi> Controller<D> {
    /// Executes an operation to a durable outcome, then closes any journal actions
    /// it left open as `action_outcome_unknown`. Only store failures are returned;
    /// operation failures are persisted on the operation itself.
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
        {
            // Repair uses this operation's context too. Let any in-flight repair
            // commit its result before recovering abandoned execution actions.
            let _guard = self.mutations.lock().await;
            self.store.interrupt_actions(Some(&operation.id)).await?;
        }
        let duration_ms = started.elapsed().as_secs_f64() * 1_000.0;
        match &result {
            Ok(outcome) => tracing::info!(outcome, duration_ms, "operation execution completed"),
            Err(error) => tracing::error!(?error, duration_ms, "operation journal update failed"),
        }
        result.map(|_| ())
    }

    /// Moves the operation to `Running`, executes it, and persists the outcome.
    ///
    /// 1. A `Requested` operation that can no longer start is reported as superseded.
    /// 2. A `Running` operation with a recorded error (a transient retry) is cycled
    ///    through `Requested` so it starts a fresh attempt.
    /// 3. A cancelled local token (shutdown or replacement) persists nothing.
    ///    Otherwise success completes the operation (or finalizes deletion), a
    ///    `Cancelled`/`Superseded` error defers to newer durable state, failed
    ///    deletions keep running with the error recorded for retry, and other
    ///    failures mark the application degraded and the operation failed.
    ///
    /// Returns a short outcome label for logging.
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
        let result = self.execute_and_cleanup(operation, cancellation).await;
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

    /// Completes convergence and removes retained secrets after service deletion.
    async fn execute_and_cleanup(
        &self,
        operation: &Operation,
        cancellation: &CancellationToken,
    ) -> Result<(), OperationError> {
        // Keep preparation and persistence state out of the discovery future.
        Box::pin(self.execute_operation(operation, cancellation)).await?;
        if operation.kind == OperationKind::Delete {
            let names = self.store.secret_names(&operation.application_id).await?;
            if !names.is_empty() {
                self.remove_secrets(operation, &names).await?;
            }
        }
        Ok(())
    }

    /// Journals retained secret removal like other runtime mutations.
    async fn remove_secrets(
        &self,
        operation: &Operation,
        names: &[String],
    ) -> Result<(), OperationError> {
        let journal = self
            .store
            .begin_action(Some(&operation.id), "remove_secrets", None)
            .await?;
        let result = match self.store.action_request(&journal, 1).await {
            Ok(()) => self
                .docker
                .remove_secrets(names, &self.ownership_labels(&operation.application_id))
                .await
                .map_err(OperationError::from),
            Err(error) => Err(error.into()),
        };
        self.store
            .finish_action(
                &journal,
                result.as_ref().err().map(OperationError::diagnostic),
            )
            .await?;
        result?;
        self.store
            .mutation_event(&operation.id)
            .await
            .map_err(OperationError::from)
    }

    /// Plans from fresh observations until no work remains. Only desired state and
    /// operation status are durable; Docker state determines the next action.
    ///
    /// The convergence deadline bounds time without progress: it restarts each
    /// time a service converges, so a dependency chain gets the full timeout per
    /// link while a single stuck service still times out.
    #[tracing::instrument(skip_all, fields(phase = "convergence"))]
    async fn execute_operation(
        &self,
        operation: &Operation,
        cancellation: &CancellationToken,
    ) -> Result<(), OperationError> {
        let request = self.operation_request(operation, cancellation).await?;
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
        if let PlanRequest::Reconcile { desired } = &request {
            self.run_jobs(operation, desired, cancellation).await?;
        }
        // Jobs have their own timeouts; convergence starts after them.
        let mut deadline = tokio::time::Instant::now() + self.retry.convergence_timeout;
        if operation.kind == OperationKind::Delete {
            self.withdraw_routes(operation, deadline).await?;
            // Job services would keep the private network attached.
            self.remove_jobs(
                operation,
                &ownership,
                crate::docker::JobRuns::All,
                cancellation,
            )
            .await?;
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
            let observed = self
                .observe_with_retry(operation, cancellation, deadline)
                .await?;
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
                {
                    let _guard = self.mutations.lock().await;
                    self.check_current(operation).await?;
                    self.store.publish_prepared(operation).await?;
                }
                // Gateway I/O has its own writer lock. Never hold the global
                // Docker mutation lock while waiting for another app's routing.
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
            self.execute_action(action, operation, &ownership, cancellation, deadline)
                .await?;
            if matches!(action.kind, piqueld_core::ActionKind::WaitForService { .. }) {
                deadline = tokio::time::Instant::now() + self.retry.convergence_timeout;
            }
        }
    }

    /// Public exposure must be withdrawn within the deletion's convergence budget.
    async fn withdraw_routes(
        &self,
        operation: &Operation,
        deadline: tokio::time::Instant,
    ) -> Result<(), OperationError> {
        tokio::time::timeout_at(deadline, async {
            self.check_current(operation).await?;
            self.sync_routes(operation, &[], true).await
        })
        .await
        .map_err(|_| OperationError::ConvergenceTimeout)?
    }

    /// Builds the plan request for an operation: a deletion request, or a
    /// reconcile request toward a freshly prepared target. Preparation is bounded
    /// by `prepare_timeout` and aborts on cancellation.
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
                    result=tokio::time::timeout(self.prepare_timeout, self.prepare_target(operation,&application))=>result.map_err(|_| OperationError::PreparationTimeout)??,
                },
            }
        })
    }

    /// Stages the application's routes and applies them through managed ingress
    /// when present (which skips the gateway while backends are not ready and no
    /// hostname is withdrawn), or acknowledges the routing table directly.
    /// `ready` tells staging whether the backing services exist yet. No-op when
    /// there are neither desired nor stored routes.
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
        // Staging rejects an operation that a newer request replaced while it
        // waited, including behind another application's gateway update.
        if let Some(ingress) = &self.ingress {
            Box::pin(ingress.apply(operation, routes, ready))
                .await
                .map_err(|error| match error.downcast_ref::<StoreError>() {
                    Some(StoreError::IllegalTransition) => OperationError::Superseded,
                    _ => OperationError::Ingress(error),
                })?;
        } else {
            self.store
                .stage_routes(
                    &operation.application_id,
                    routes,
                    ready,
                    Some(&operation.id),
                )
                .await
                .map_err(|error| match error {
                    StoreError::IllegalTransition => OperationError::Superseded,
                    other => other.into(),
                })?;
            let table = self.store.routing_table().await?;
            self.store.acknowledge_routes(&table).await?;
        }
        Ok(())
    }

    /// Records the planning phase (naming the first blocking resource, if any) and
    /// rejects blocked plans with their classified error.
    pub(super) async fn check_plan(
        &self,
        operation: &Operation,
        plan: &Plan,
    ) -> Result<(), OperationError> {
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

    /// Resolves the operation's deployable target, reusing a previously saved one.
    ///
    /// Verifies the swarm topology, fetches the deployment manifest, keeps the
    /// application's current name, reuses prior image resolutions unless this is
    /// a `Refresh`, pins secret versions, then resolves images and builds sources.
    /// The result is saved on the operation only if it is still current and the
    /// topology is still supported.
    #[tracing::instrument(skip_all, fields(phase = "preparation", timeout_seconds = self.prepare_timeout.as_secs()))]
    async fn prepare_target(
        &self,
        operation: &Operation,
        application: &super::StoredApplication,
    ) -> Result<piqueld_core::ResolvedApplication, OperationError> {
        self.check_current(operation).await?;
        self.docker.ensure_swarm(false).await?;
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
        let mut reusable = if operation.kind == OperationKind::Refresh {
            piqueld_core::ResolutionSet::default()
        } else {
            application
                .resolved
                .as_ref()
                .map_or_else(piqueld_core::ResolutionSet::default, |target| {
                    target.reusable_resolutions(&manifest)
                })
        };
        reusable.secret_names = self.store.pin_secrets(&operation.id, &manifest).await?;
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
        // The topology may have changed while images were pulled or built.
        self.docker.ensure_swarm(false).await?;
        self.store
            .save_prepared(operation, &prepared)
            .await
            .map_err(OperationError::from)?;
        Ok(prepared)
    }

    /// Fails with `Superseded` when a newer operation exists for the application,
    /// or `Cancelled` when this operation is no longer `Running`.
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

    /// Marks the application degraded with the failure message.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::{Mutation, MutationResponse},
        docker::BollardDocker,
        store::Store,
    };

    /// Accepts a routed application and starts executing its operation.
    async fn running_operation(store: &Store) -> Operation {
        let manifest = piqueld_core::parse_toml(&format!(
            "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='routed'\n\
             [[spec.services]]\nname='web'\n[spec.services.source]\ntype='image'\nimage='{}'\n\
             [[spec.routes]]\nhostname='routed.example.com'\nservice='web'\nport=80",
            crate::ingress::CADDY_IMAGE
        ))
        .unwrap();
        let (MutationResponse::Saved(saved), _) = store
            .accept(Mutation::save(manifest, None, true), Some(0), false, None)
            .await
            .unwrap()
        else {
            panic!("saved deployment")
        };
        let operation_id = saved.operation_id.unwrap();
        store
            .transition_operation(
                &operation_id,
                OperationState::Requested,
                OperationState::Running,
                None,
            )
            .await
            .unwrap();
        store.operation(&operation_id).await.unwrap()
    }

    #[tokio::test]
    async fn routing_superseded_while_waiting_for_the_gateway_is_superseded() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(temp.path().join("db")).await.unwrap());
        let operation = running_operation(&store).await;
        let routes = store
            .get(&operation.application_id)
            .await
            .unwrap()
            .application
            .spec()
            .routes
            .clone();
        let socket_path = temp.path().join("unused.sock");
        let _socket = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let ingress = Arc::new(
            crate::ingress::Ingress::new(true, &socket_path, temp.path(), Arc::clone(&store))
                .unwrap(),
        );
        let controller = Arc::new(
            Controller::new(
                Arc::new(BollardDocker::connect(&socket_path).unwrap()),
                Arc::clone(&store),
            )
            .with_ingress(Arc::clone(&ingress)),
        );

        let gateway = ingress.hold_updates().await;
        let staging = tokio::spawn({
            let operation = operation.clone();
            async move { controller.sync_routes(&operation, &routes, true).await }
        });
        // The routing phase commits just before staging waits for the gateway.
        while store
            .operation(&operation.id)
            .await
            .unwrap()
            .phase
            .as_deref()
            != Some("routing")
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        store
            .accept(
                Mutation::deploy(operation.application_id.clone()),
                None,
                true,
                None,
            )
            .await
            .unwrap();
        drop(gateway);
        assert!(matches!(
            staging.await.unwrap(),
            Err(OperationError::Superseded)
        ));
    }

    #[tokio::test]
    async fn reconciliation_leaves_concurrent_daemon_actions_open() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(temp.path().join("db")).await.unwrap());
        let operation = running_operation(&store).await;
        store
            .transition_operation(
                &operation.id,
                OperationState::Running,
                OperationState::Requested,
                None,
            )
            .await
            .unwrap();
        let socket_path = temp.path().join("unused.sock");
        let _socket = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let controller = Controller::new(
            Arc::new(BollardDocker::connect(&socket_path).unwrap()),
            Arc::clone(&store),
        );
        // Another worker, such as ingress, is mid-change when reconciliation starts.
        let action = store
            .begin_action(None, "ingress_start_gateway", None)
            .await
            .unwrap();
        let cancellation = CancellationToken::new();
        let run = controller.run(
            Arc::new(tokio::sync::Notify::new()),
            std::time::Duration::from_mins(1),
            0,
            0,
            cancellation.clone(),
        );
        let finished = async {
            // Operations start only after the first discovery pass, including recovery.
            while store.operation(&operation.id).await.unwrap().state == OperationState::Requested {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let finished = store.finish_action(&action, None).await;
            cancellation.cancel();
            finished
        };
        let (result, finished) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(run, finished)
        })
        .await
        .unwrap();
        result.unwrap();
        finished.unwrap();
    }

    #[tokio::test]
    async fn execution_cleanup_waits_for_live_repair_to_commit() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(temp.path().join("db")).await.unwrap());
        let manifest = piqueld_core::parse_toml(
            "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='repair'\n[spec]",
        ).unwrap();
        let (MutationResponse::Saved(saved), _) = store
            .accept(Mutation::save(manifest, None, true), Some(0), false, None)
            .await
            .unwrap()
        else {
            panic!("saved deployment")
        };
        let operation_id = saved.operation_id.unwrap();
        let operation = store.operation(&operation_id).await.unwrap();
        // A stale execution takes its normal early-exit path without calling Docker.
        store
            .transition_operation(
                &operation.id,
                OperationState::Requested,
                OperationState::Running,
                None,
            )
            .await
            .unwrap();
        store
            .transition_operation(
                &operation.id,
                OperationState::Running,
                OperationState::Succeeded,
                None,
            )
            .await
            .unwrap();
        let socket_path = temp.path().join("unused.sock");
        let _socket = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let controller = Controller::new(
            Arc::new(BollardDocker::connect(&socket_path).unwrap()),
            Arc::clone(&store),
        );
        let abandoned = store
            .begin_action(Some(&operation.id), "resolving_image", Some("web"))
            .await
            .unwrap();
        let guard = controller.mutations.lock().await;
        let repair = store
            .begin_action(Some(&operation.id), "ensure_service", Some("web"))
            .await
            .unwrap();
        store.action_request(&repair, 1).await.unwrap();
        let cancellation = CancellationToken::new();
        let cleanup = controller.run_operation(&operation, &cancellation);
        tokio::pin!(cleanup);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut cleanup)
                .await
                .is_err()
        );
        store.finish_action(&repair, None).await.unwrap();
        drop(guard);
        tokio::time::timeout(std::time::Duration::from_secs(1), cleanup)
            .await
            .unwrap()
            .unwrap();
        let events = store
            .events(Some(&operation.application_id), None, 100)
            .await
            .unwrap()
            .items;
        assert!(events.iter().any(|e| e.action_id.as_deref() == Some(&repair.id) && e.kind == "action_succeeded"));
        assert!(
            !events
                .iter()
                .any(|e| e.action_id.as_deref() == Some(&repair.id)
                    && e.kind == "action_outcome_unknown")
        );
        assert!(
            events
                .iter()
                .any(|e| e.action_id.as_deref() == Some(&abandoned.id)
                    && e.kind == "action_outcome_unknown")
        );
    }
}
