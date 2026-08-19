use super::handler::journal_error;
use super::{
    ActionKind, ApplicationRepository, CancellationToken, Convergence, DockerApi, DockerError,
    Duration, OperationError, PlanAction, ReconcileHandler,
};

impl<D: DockerApi> ReconcileHandler<D> {
    pub(super) async fn execute_action(
        &self,
        action: &PlanAction,
        app: &piqueld_core::ApplicationId,
        cancellation: &CancellationToken,
    ) -> Result<(), OperationError> {
        match &action.kind {
            ActionKind::EnsureNetwork { network } => {
                self.retry(cancellation, || self.docker.ensure_network(network))
                    .await
            }
            ActionKind::EnsureVolume { volume } => {
                self.retry(cancellation, || self.docker.ensure_volume(volume))
                    .await
            }
            ActionKind::EnsureService { service } => {
                self.retry(cancellation, || self.docker.ensure_service(service))
                    .await
            }
            ActionKind::RemoveService { name } => {
                let labels = self.ownership_labels(app).await?;
                self.retry(cancellation, || self.docker.remove_service(name, &labels))
                    .await
            }
            ActionKind::RemoveNetwork { name } => {
                let labels = self.ownership_labels(app).await?;
                self.retry(cancellation, || self.docker.remove_network(name, &labels))
                    .await
            }
            ActionKind::WaitForService { service } => {
                self.wait_service(app, service, false, cancellation).await
            }
            ActionKind::WaitForServiceRemoval { service } => {
                self.wait_service(app, service, true, cancellation).await
            }
            ActionKind::RetainVolume { .. } | ActionKind::ResolveImage { .. } => Ok(()),
            ActionKind::EnsureSecret { .. }
            | ActionKind::RemoveSecret { .. }
            | ActionKind::WaitForSecretUnused { .. }
            | ActionKind::AwaitSecretGeneration { .. } => {
                Err(OperationError::SecretLifecycleUnavailable)
            }
            ActionKind::ResolveGit { .. }
            | ActionKind::BuildImage { .. }
            | ActionKind::PushImage { .. } => Err(OperationError::BuildPipelineUnavailable),
        }
    }
    pub(super) async fn ownership_labels(
        &self,
        app: &piqueld_core::ApplicationId,
    ) -> Result<std::collections::BTreeMap<String, String>, OperationError> {
        let stored = self.store.get(app).await.map_err(journal_error)?;
        let resolved = stored
            .resolved
            .ok_or(OperationError::ResolvedStateMissing)?;
        Ok(std::collections::BTreeMap::from([
            ("io.piqueld.managed".into(), "true".into()),
            (
                "io.piqueld.instance".into(),
                resolved.instance_id.to_string(),
            ),
            ("io.piqueld.application".into(), app.to_string()),
        ]))
    }
    pub(super) async fn retry<F, Fut>(
        &self,
        cancellation: &CancellationToken,
        mut call: F,
    ) -> Result<(), OperationError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<(), DockerError>>,
    {
        let mut delay = self.retry.initial_delay;
        for attempt in 0..self.retry.attempts {
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            match call().await {
                Ok(()) => return Ok(()),
                Err(
                    error @ (DockerError::OwnershipConflict
                    | DockerError::ConfigurationConflict
                    | DockerError::NotManager
                    | DockerError::IncompatibleSwarm),
                ) => {
                    tracing::error!(error = ?error, "Docker operation rejected");
                    return Err(error.into());
                }
                Err(error) if attempt + 1 == self.retry.attempts => {
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
                    delay = (delay * 2).min(self.retry.max_delay);
                }
            }
        }
        Err(OperationError::DockerRequestFailed(
            "execute retryable Docker operation",
        ))
    }
    pub(super) async fn wait_service(
        &self,
        app: &piqueld_core::ApplicationId,
        name: &str,
        removed: bool,
        cancellation: &CancellationToken,
    ) -> Result<(), OperationError> {
        let deadline = tokio::time::Instant::now() + self.retry.convergence_timeout;
        loop {
            let observed = self.docker.observe(app).await?;
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
}
