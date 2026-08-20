use super::{
    ActionKind, CancellationToken, Convergence, DockerApi, DockerError, Duration, OperationError,
    PlanAction, ReconcileHandler,
};
use piqueld_core::ApplicationId;

impl<D: DockerApi> ReconcileHandler<D> {
    pub(super) async fn execute_action(
        &self,
        action: &PlanAction,
        app: &ApplicationId,
        ownership: &std::collections::BTreeMap<String, String>,
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
                self.retry(cancellation, || self.docker.remove_service(name, ownership))
                    .await
            }
            ActionKind::RemoveNetwork { name } => {
                self.retry(cancellation, || self.docker.remove_network(name, ownership))
                    .await
            }
            ActionKind::WaitForService { service } => {
                self.wait_service(app, service, false, cancellation).await
            }
            ActionKind::WaitForServiceRemoval { service } => {
                self.wait_service(app, service, true, cancellation).await
            }
            ActionKind::RetainVolume { .. } | ActionKind::ResolveImage { .. } => Ok(()),
        }
    }
    pub(super) fn ownership_labels(
        application: &crate::store::StoredApplication,
    ) -> std::collections::BTreeMap<String, String> {
        let resolved = &application.resolved;
        std::collections::BTreeMap::from([
            (super::MANAGED_LABEL.into(), "true".into()),
            (
                super::INSTANCE_LABEL.into(),
                resolved.instance_id.to_string(),
            ),
            (
                super::APPLICATION_LABEL.into(),
                application.application.id.to_string(),
            ),
        ])
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
        let attempts = self.retry.attempts.max(1);
        let mut delay = self.retry.initial_delay;
        for attempt in 0..attempts {
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            match call().await {
                Ok(()) => return Ok(()),
                Err(
                    error @ (DockerError::OwnershipConflict
                    | DockerError::ConfigurationConflict
                    | DockerError::NotManager
                    | DockerError::IncompatibleSwarm
                    | DockerError::Validation(_)),
                ) => {
                    tracing::error!(error = %error, "Docker operation rejected");
                    return Err(error.into());
                }
                Err(error) if attempt + 1 == attempts => {
                    tracing::error!(error = %error, "Docker operation failed after retries");
                    return Err(error.into());
                }
                Err(error) => {
                    tracing::warn!(
                        error = %error,
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
        app: &piqueld_core::ApplicationId,
        name: &str,
        removed: bool,
        cancellation: &CancellationToken,
    ) -> Result<(), OperationError> {
        let deadline = tokio::time::Instant::now() + self.retry.convergence_timeout;
        loop {
            let observed = self.observe_with_retry(app, cancellation, deadline).await?;
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
        app: &piqueld_core::ApplicationId,
        cancellation: &CancellationToken,
        deadline: tokio::time::Instant,
    ) -> Result<piqueld_core::ObservedApplication, OperationError> {
        let mut delay = self.retry.initial_delay;
        loop {
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            match self.docker.observe(app).await {
                Ok(observed) => return Ok(observed),
                Err(
                    error @ (DockerError::OwnershipConflict
                    | DockerError::ConfigurationConflict
                    | DockerError::NotManager
                    | DockerError::IncompatibleSwarm),
                ) => {
                    tracing::error!(error = %error, "Docker observation rejected");
                    return Err(error.into());
                }
                Err(error) => {
                    let now = tokio::time::Instant::now();
                    if now >= deadline {
                        tracing::error!(error = %error, "Docker observation failed after retries");
                        return Err(error.into());
                    }
                    let remaining = deadline.saturating_duration_since(now);
                    tracing::warn!(error = %error, "Docker observation failed; retrying");
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
