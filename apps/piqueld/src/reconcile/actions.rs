use super::{
    ActionKind, CancellationToken, Controller, Convergence, DockerApi, DockerError, Duration,
    Operation, OperationError,
};

/// Decrypted values keyed by their immutable Docker secret names.
pub(super) type SecretValues = Vec<(String, zeroize::Zeroizing<Vec<u8>>)>;

impl<D: DockerApi> Controller<D> {
    #[tracing::instrument(skip_all, fields(action = action.kind.name(), resource = action.kind.resource_name()))]
    pub(super) async fn execute_action(
        &self,
        action: &piqueld_core::PlanAction,
        operation: &Operation,
        ownership: &std::collections::BTreeMap<String, String>,
        cancellation: &CancellationToken,
        deadline: tokio::time::Instant,
    ) -> Result<(), OperationError> {
        self.store
            .progress(
                &operation.id,
                action.kind.name(),
                Some(action.kind.resource_name()),
            )
            .await
            .map_err(OperationError::from)?;
        let journal = self
            .store
            .begin_action(
                Some(&operation.id),
                action.kind.name(),
                Some(action.kind.resource_name()),
            )
            .await?;
        let started = std::time::Instant::now();
        tracing::debug!("action started");
        // Actions enforce the deadline themselves so a timeout closes the action as
        // a failure instead of leaving it for interrupted-action recovery.
        let result = match &action.kind {
            kind if kind.mutates_runtime() => match self.service_secrets(kind, ownership).await {
                Ok(secrets) => tokio::time::timeout_at(
                    deadline,
                    self.retry(operation, &journal, cancellation, || {
                        self.mutate_action(kind, ownership, &secrets)
                    }),
                )
                .await
                .unwrap_or(Err(OperationError::ConvergenceTimeout)),
                Err(error) => Err(error),
            },
            ActionKind::WaitForService { service } => {
                self.wait_service(operation, service, false, cancellation, deadline)
                    .await
            }
            ActionKind::WaitForServiceRemoval { service } => {
                self.wait_service(operation, service, true, cancellation, deadline)
                    .await
            }
            _ => Ok(()),
        };
        self.store
            .finish_action(
                &journal,
                result.as_ref().err().map(OperationError::diagnostic),
            )
            .await?;
        tracing::debug!(
            succeeded = result.is_ok(),
            duration_ms = started.elapsed().as_secs_f64() * 1_000.0,
            "action completed"
        );
        if result.is_ok() && action.kind.mutates_runtime() {
            self.store
                .mutation_event(&operation.id)
                .await
                .map_err(OperationError::from)?;
        }
        result
    }
    /// Decrypts a service's pinned secret versions before any Docker request, so key
    /// failures are classified as secret storage rather than runtime failures.
    pub(super) async fn service_secrets(
        &self,
        kind: &ActionKind,
        ownership: &std::collections::BTreeMap<String, String>,
    ) -> Result<SecretValues, OperationError> {
        let ActionKind::EnsureService { service } = kind else {
            return Ok(Vec::new());
        };
        if service.secrets.is_empty() {
            return Ok(Vec::new());
        }
        let app = ownership
            .get(super::APPLICATION_LABEL)
            .and_then(|app| piqueld_core::ApplicationId::parse(app).ok())
            .ok_or(OperationError::OwnershipConflict)?;
        let mut values = Vec::with_capacity(service.secrets.len());
        for secret in &service.secrets {
            let value = self
                .store
                .secret_plaintext(&app, &secret.secret_name)
                .await?;
            values.push((secret.secret_name.clone(), value));
        }
        Ok(values)
    }

    pub(super) async fn mutate_action(
        &self,
        kind: &ActionKind,
        ownership: &std::collections::BTreeMap<String, String>,
        secrets: &SecretValues,
    ) -> Result<(), DockerError> {
        self.docker.ensure_swarm(false).await?;
        match kind {
            ActionKind::EnsureNetwork { network } => self.docker.ensure_network(network).await,
            ActionKind::EnsureVolume { volume } => self.docker.ensure_volume(volume).await,
            ActionKind::EnsureService { service } => {
                for (name, value) in secrets {
                    self.docker.ensure_secret(name, value, ownership).await?;
                }
                self.docker.ensure_service(service).await
            }
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
        journal: &crate::store::JournalAction,
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
                self.store.action_request(journal, attempt + 1).await?;
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
                    let diagnostic = OperationError::from(error).diagnostic();
                    self.store
                        .action_retry(journal, attempt + 1, delay, &diagnostic)
                        .await?;
                    tracing::warn!(
                        diagnostic_id = %diagnostic.id,
                        attempt = attempt + 1,
                        attempts,
                        retry_delay_ms = delay.as_secs_f64() * 1_000.0,
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
        deadline: tokio::time::Instant,
    ) -> Result<(), OperationError> {
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
            let poll = Duration::from_millis(250)
                .min(deadline.saturating_duration_since(tokio::time::Instant::now()));
            tokio::select! {()=cancellation.cancelled()=>return Err(OperationError::Cancelled),()=tokio::time::sleep(poll)=>{}}
        }
    }

    /// Reads application state until Docker responds or the convergence deadline expires.
    /// A failed observation is journaled as one action and closed on every exit.
    #[tracing::instrument(skip_all, fields(phase = "observation"))]
    pub(super) async fn observe_with_retry(
        &self,
        operation: &Operation,
        cancellation: &CancellationToken,
        deadline: tokio::time::Instant,
    ) -> Result<piqueld_core::ObservedApplication, OperationError> {
        let mut journal = None;
        let result = tokio::time::timeout_at(
            deadline,
            self.observe_attempts(operation, cancellation, deadline, &mut journal),
        )
        .await
        .unwrap_or(Err(OperationError::ConvergenceTimeout));
        if let Some(action) = &journal {
            self.store
                .finish_action(
                    action,
                    result.as_ref().err().map(OperationError::diagnostic),
                )
                .await?;
        }
        result
    }

    async fn observe_attempts(
        &self,
        operation: &Operation,
        cancellation: &CancellationToken,
        deadline: tokio::time::Instant,
        journal: &mut Option<crate::store::JournalAction>,
    ) -> Result<piqueld_core::ObservedApplication, OperationError> {
        let mut delay = self.retry.initial_delay;
        let mut attempt = 0_u32;
        loop {
            attempt = attempt.saturating_add(1);
            self.check_current(operation).await?;
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            let error = match self.docker.observe(&operation.application_id).await {
                Ok(observed) => return Ok(observed),
                Err(error) => error,
            };
            if journal.is_none() {
                *journal = Some(
                    self.store
                        .begin_action(Some(&operation.id), "observation", None)
                        .await?,
                );
            }
            let action = journal.as_ref().expect("failed observation has an action");
            let terminal = matches!(
                error,
                DockerError::OwnershipConflict
                    | DockerError::ConfigurationConflict
                    | DockerError::NotManager
                    | DockerError::IncompatibleSwarm
                    | DockerError::Validation(_)
            );
            let error = OperationError::from(error);
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if terminal || remaining.is_zero() {
                return Err(error);
            }
            let diagnostic = error.diagnostic();
            self.store
                .action_retry(action, attempt, delay.min(remaining), &diagnostic)
                .await?;
            tracing::warn!(diagnostic_id=%diagnostic.id,attempt,retry_delay_ms=delay.min(remaining).as_secs_f64()*1000.0,"Docker observation failed; retrying");
            tokio::select! {()=cancellation.cancelled()=>return Err(OperationError::Cancelled),()=tokio::time::sleep(delay.min(remaining))=>{}}
            delay = delay.saturating_mul(2).min(self.retry.max_delay);
        }
    }
}
