use super::{
    ActionKind, CancellationToken, Controller, Convergence, DockerApi, DockerError, Duration,
    Operation, OperationError,
};
use crate::store::JournalAction;

/// Decrypted values keyed by their immutable Docker secret names.
pub(super) type SecretValues = Vec<(String, zeroize::Zeroizing<Vec<u8>>)>;
/// First delay between observations while waiting for a service; it
/// doubles up to [`SERVICE_POLL_MAX`].
const SERVICE_POLL: Duration = Duration::from_millis(250);
/// Longest delay between observations while waiting for a service. Each is
/// a full observation of the environment.
const SERVICE_POLL_MAX: Duration = Duration::from_secs(1);

impl<D: DockerApi> Controller<D> {
    /// Executes one planned action inside its own journal entry.
    ///
    /// Records the operation phase, opens the action, then either runs a runtime
    /// mutation with retries (after decrypting any service secrets) or waits for a
    /// service to converge or disappear. Non-mutating actions such as volume
    /// retention succeed immediately. The action is closed with the outcome before
    /// returning, and successful mutations emit a `resource_mutated` event.
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
            .await?;
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
            kind if kind.mutates_runtime() => {
                match self.service_secrets(kind.secrets(), ownership).await {
                    Ok(secrets) => tokio::time::timeout_at(
                        deadline,
                        self.retry(operation, &journal, cancellation, || {
                            self.mutate_action(kind, ownership, &secrets)
                        }),
                    )
                    .await
                    .unwrap_or(Err(OperationError::ConvergenceTimeout)),
                    Err(error) => Err(error),
                }
            }
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
        let result = self.close_action(&journal, result).await;
        tracing::debug!(
            succeeded = result.is_ok(),
            duration_ms = started.elapsed().as_secs_f64() * 1_000.0,
            "action completed"
        );
        if result.is_ok() && action.kind.mutates_runtime() {
            self.store.mutation_event(&operation.id).await?;
        }
        result
    }

    /// Closes `journal` with `result`'s outcome, then returns `result`.
    /// Failing to record the outcome fails instead.
    pub(super) async fn close_action<T>(
        &self,
        journal: &JournalAction,
        result: Result<T, OperationError>,
    ) -> Result<T, OperationError> {
        self.store
            .finish_action(
                journal,
                result.as_ref().err().map(OperationError::diagnostic),
            )
            .await?;
        result
    }

    /// Journals `request` as the single attempt of a new `phase` action of
    /// `operation` on `resource`: its intent is committed before it runs,
    /// and its outcome closes the action.
    pub(super) async fn journaled<T>(
        &self,
        operation: &str,
        phase: &str,
        resource: Option<&str>,
        request: impl Future<Output = Result<T, OperationError>>,
    ) -> Result<T, OperationError> {
        let journal = self
            .store
            .begin_action(Some(operation), phase, resource)
            .await?;
        let result = self.request_once(&journal, request).await;
        self.close_action(&journal, result).await
    }

    /// Commits the single attempt of the open `journal` action, then sends
    /// `request`. The request is not sent when its intent cannot be recorded.
    pub(super) async fn request_once<T>(
        &self,
        journal: &JournalAction,
        request: impl Future<Output = Result<T, OperationError>>,
    ) -> Result<T, OperationError> {
        match self.store.action_request(journal, 1).await {
            Ok(()) => request.await,
            Err(error) => Err(error.into()),
        }
    }

    /// Sleeps for `delay`, or fails with `Cancelled` once `cancellation` fires.
    pub(super) async fn pause(
        cancellation: &CancellationToken,
        delay: Duration,
    ) -> Result<(), OperationError> {
        tokio::select! {
            () = cancellation.cancelled() => Err(OperationError::Cancelled),
            () = tokio::time::sleep(delay) => Ok(()),
        }
    }

    /// Decrypts pinned secret versions before any Docker request, so key
    /// failures are classified as secret storage rather than runtime failures.
    pub(super) async fn service_secrets(
        &self,
        secrets: &[piqueld_core::resource::SecretFile],
        ownership: &std::collections::BTreeMap<String, String>,
    ) -> Result<SecretValues, OperationError> {
        if secrets.is_empty() {
            return Ok(Vec::new());
        }
        let app = ownership
            .get(super::APPLICATION_LABEL)
            .and_then(|app| piqueld_core::EnvironmentId::parse(app).ok())
            .ok_or(OperationError::OwnershipConflict)?;
        let mut values = Vec::with_capacity(secrets.len());
        for secret in secrets {
            let value = self
                .store
                .secret_plaintext(&app, &secret.secret_name)
                .await?;
            values.push((secret.secret_name.clone(), value));
        }
        Ok(values)
    }

    /// Performs one attempt of a mutating action. Non-removal actions
    /// first verify the supported swarm topology, and `EnsureService` first
    /// ensures each pinned secret so the service can reference it.
    pub(super) async fn mutate_action(
        &self,
        kind: &ActionKind,
        ownership: &std::collections::BTreeMap<String, String>,
        secrets: &SecretValues,
    ) -> Result<(), DockerError> {
        // Creating or updating resources requires the supported topology so local
        // images and volumes cannot move. Removals stay safe on any topology.
        if !matches!(
            kind,
            ActionKind::RemoveService { .. } | ActionKind::RemoveNetwork { .. }
        ) {
            self.docker.ensure_swarm(false).await?;
        }
        match kind {
            ActionKind::EnsureNetwork { network } => {
                self.docker.ensure_network(network).await?;
                // The exporter attaches to a link network only once it exists.
                if network
                    .labels
                    .contains_key(piqueld_core::resource::LINK_LABEL)
                {
                    self.relink.notify_one();
                }
                Ok(())
            }
            ActionKind::EnsureVolume { volume } => self.docker.ensure_volume(volume).await,
            ActionKind::EnsureService { service } => {
                for (name, value) in secrets {
                    self.docker.ensure_secret(name, value, ownership).await?;
                }
                // Under the mutation lock, as is telemetry cleanup.
                if let (Some(sockets), Some(mount)) = (&self.sockets, &service.telemetry) {
                    sockets.prepare(mount);
                }
                self.docker.ensure_service(service).await
            }
            ActionKind::RemoveService { name } => self.docker.remove_service(name, ownership).await,
            ActionKind::RemoveNetwork { name } => self.docker.remove_network(name, ownership).await,
            _ => Err(DockerError::Validation("execute a non-mutating action")),
        }
    }

    /// Labels that mark Docker resources as managed by this instance and
    /// environment `id`; see [`piqueld_core::resource::Ownership::scope`].
    pub(super) fn ownership_labels(
        &self,
        id: &piqueld_core::EnvironmentId,
    ) -> std::collections::BTreeMap<String, String> {
        piqueld_core::resource::Ownership::scope(self.store.instance_id(), id)
    }

    /// Runs a journaled Docker request with exponential backoff.
    ///
    /// Each attempt re-checks that the operation is still current, both before and
    /// after taking the global mutation lock, and records the request attempt in the
    /// journal. Ownership, configuration, swarm, and validation errors are terminal;
    /// other errors are journaled as retries until attempts run out. Cancellation
    /// interrupts the backoff sleep.
    pub(super) async fn retry<F, Fut>(
        &self,
        operation: &Operation,
        journal: &JournalAction,
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
                Err(error) if error.is_terminal() => {
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
                    Self::pause(cancellation, delay).await?;
                    delay = delay.saturating_mul(2).min(self.retry.max_delay);
                }
            }
        }
        Err(OperationError::DockerRequestFailed(
            "execute retryable Docker operation",
        ))
    }

    /// Polls observations, from every [`SERVICE_POLL`] backing off to every
    /// [`SERVICE_POLL_MAX`], until the named service has converged (or, with
    /// `removed`, no longer exists). A `Failed` convergence ends the wait with
    /// `ServiceUpdateFailed`; the deadline yields `ConvergenceTimeout`.
    pub(super) async fn wait_service(
        &self,
        operation: &Operation,
        name: &str,
        removed: bool,
        cancellation: &CancellationToken,
        deadline: tokio::time::Instant,
    ) -> Result<(), OperationError> {
        let mut poll = SERVICE_POLL;
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
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            Self::pause(cancellation, poll.min(remaining)).await?;
            poll = poll.saturating_mul(2).min(SERVICE_POLL_MAX);
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
        match &journal {
            Some(action) => self.close_action(action, result).await,
            None => result,
        }
    }

    /// Retry loop behind `observe_with_retry`. The journal action is opened lazily
    /// on the first failure, so successful observations leave no journal trace.
    /// Backoff is clamped to the time remaining before the deadline.
    async fn observe_attempts(
        &self,
        operation: &Operation,
        cancellation: &CancellationToken,
        deadline: tokio::time::Instant,
        journal: &mut Option<JournalAction>,
    ) -> Result<piqueld_core::ObservedApplication, OperationError> {
        let mut delay = self.retry.initial_delay;
        let mut attempt = 0_u32;
        loop {
            attempt = attempt.saturating_add(1);
            self.check_current(operation).await?;
            if cancellation.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            let error = match self.docker.observe(&operation.environment_id).await {
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
            let terminal = error.is_terminal();
            let error = OperationError::from(error);
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if terminal || remaining.is_zero() {
                return Err(error);
            }
            let diagnostic = error.diagnostic();
            let backoff = delay.min(remaining);
            self.store
                .action_retry(action, attempt, backoff, &diagnostic)
                .await?;
            tracing::warn!(
                diagnostic_id = %diagnostic.id,
                attempt,
                retry_delay_ms = backoff.as_secs_f64() * 1000.0,
                "Docker observation failed; retrying"
            );
            Self::pause(cancellation, backoff).await?;
            delay = delay.saturating_mul(2).min(self.retry.max_delay);
        }
    }
}
