use super::{
    ActionKind, CancellationToken, Controller, Convergence, DockerApi, DockerError, Duration,
    Operation, OperationError,
};

impl<D: DockerApi> Controller<D> {
    pub(super) async fn execute_action(
        &self,
        action: &piqueld_core::PlanAction,
        operation: &Operation,
        ownership: &std::collections::BTreeMap<String, String>,
        cancellation: &CancellationToken,
    ) -> Result<(), OperationError> {
        self.store
            .progress(
                &operation.id,
                action.kind.name(),
                Some(action.kind.resource_name()),
            )
            .await
            .map_err(OperationError::from)?;
        let result = match &action.kind {
            kind if kind.mutates_runtime() => {
                self.retry(operation, cancellation, || {
                    self.mutate_action(kind, ownership)
                })
                .await
            }
            ActionKind::WaitForService { service } => {
                self.wait_service(operation, service, false, cancellation)
                    .await
            }
            ActionKind::WaitForServiceRemoval { service } => {
                self.wait_service(operation, service, true, cancellation)
                    .await
            }
            _ => Ok(()),
        };
        if result.is_ok() && action.kind.mutates_runtime() {
            self.store
                .mutation_event(&operation.id)
                .await
                .map_err(OperationError::from)?;
        }
        result
    }
    pub(super) async fn mutate_action(
        &self,
        kind: &ActionKind,
        ownership: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        match kind {
            ActionKind::EnsureNetwork { network } => self.docker.ensure_network(network).await,
            ActionKind::EnsureVolume { volume } => self.docker.ensure_volume(volume).await,
            ActionKind::EnsureService { service } => self.docker.ensure_service(service).await,
            ActionKind::RemoveService { name } => self.docker.remove_service(name, ownership).await,
            ActionKind::RemoveNetwork { name } => self.docker.remove_network(name, ownership).await,
            _ => Err(DockerError::Validation("execute a non-mutating action")),
        }
    }

    pub(super) fn ownership_labels(
        &self,
        id: &piqueld_core::ApplicationId,
    ) -> std::collections::BTreeMap<String, String> {
        std::collections::BTreeMap::from([
            (super::MANAGED_LABEL.into(), "true".into()),
            (
                super::INSTANCE_LABEL.into(),
                self.store.instance_id().to_owned(),
            ),
            (super::APPLICATION_LABEL.into(), id.to_string()),
        ])
    }
    pub(super) async fn retry<F, Fut>(
        &self,
        operation: &Operation,
        cancellation: &CancellationToken,
        mut call: F,
    ) -> Result<(), OperationError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<(), DockerError>>,
    {
        let attempts = self.retry.attempts.max(1);
        let mut delay = self.retry.initial_delay;
        for attempt in 0..attempts {
            self.check_current(operation).await?;
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            let result = {
                let _guard = self.mutations.lock().await;
                self.check_current(operation).await?;
                call().await
            };
            match result {
                Ok(()) => return Ok(()),
                Err(
                    error @ (DockerError::OwnershipConflict
                    | DockerError::ConfigurationConflict
                    | DockerError::NotManager
                    | DockerError::IncompatibleSwarm
                    | DockerError::Validation(_)),
                ) => {
                    tracing::error!(error = ?error, "Docker operation rejected");
                    return Err(error.into());
                }
                Err(error) if attempt + 1 == attempts => {
                    tracing::error!(error = ?error, "Docker operation failed after retries");
                    return Err(error.into());
                }
                Err(error) => {
                    tracing::warn!(
                        error = ?error,
                        attempt = attempt + 1,
                        "Docker operation failed; retrying"
                    );
                    tokio::select! {()=cancellation.cancelled()=>return Err(OperationError::Cancelled),()=tokio::time::sleep(delay)=>{}}
                    delay = delay.saturating_mul(2).min(self.retry.max_delay);
                }
            }
        }
        Err(OperationError::DockerRequestFailed(
            "execute retryable Docker operation",
        ))
    }
    pub(super) async fn wait_service(
        &self,
        operation: &Operation,
        name: &str,
        removed: bool,
        cancellation: &CancellationToken,
    ) -> Result<(), OperationError> {
        let deadline = tokio::time::Instant::now() + self.retry.convergence_timeout;
        loop {
            let observed = self
                .observe_with_retry(operation, cancellation, deadline)
                .await?;
            match observed.services.iter().find(|s| s.name == name) {
                None if removed => return Ok(()),
                Some(s) if !removed && s.convergence == Convergence::Converged => return Ok(()),
                Some(s) if !removed && s.convergence == Convergence::Failed => {
                    return Err(OperationError::ServiceUpdateFailed);
                }
                _ => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(OperationError::ConvergenceTimeout);
            }
            tokio::select! {()=cancellation.cancelled()=>return Err(OperationError::Cancelled),()=tokio::time::sleep(Duration::from_millis(250))=>{}}
        }
    }

    /// Reads application state until Docker responds or the convergence deadline expires.
    pub(super) async fn observe_with_retry(
        &self,
        operation: &Operation,
        cancellation: &CancellationToken,
        deadline: tokio::time::Instant,
    ) -> Result<piqueld_core::ObservedApplication, OperationError> {
        let mut delay = self.retry.initial_delay;
        loop {
            self.check_current(operation).await?;
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            match self.docker.observe(&operation.application_id).await {
                Ok(observed) => return Ok(observed),
                Err(
                    error @ (DockerError::OwnershipConflict
                    | DockerError::ConfigurationConflict
                    | DockerError::NotManager
                    | DockerError::IncompatibleSwarm
                    | DockerError::Validation(_)),
                ) => {
                    tracing::error!(error = ?error, "Docker observation rejected");
                    return Err(error.into());
                }
                Err(error) => {
                    let now = tokio::time::Instant::now();
                    if now >= deadline {
                        tracing::error!(error = ?error, "Docker observation failed after retries");
                        return Err(error.into());
                    }
                    let remaining = deadline.saturating_duration_since(now);
                    tracing::warn!(error = ?error, "Docker observation failed; retrying");
                    tokio::select! {
                        () = cancellation.cancelled() => return Err(OperationError::Cancelled),
                        () = tokio::time::sleep(delay.min(remaining)) => {}
                    }
                    delay = delay.saturating_mul(2).min(self.retry.max_delay);
                }
            }
        }
    }
}
