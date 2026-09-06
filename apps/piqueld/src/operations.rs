//! Bounded, cancellation-aware scheduling for durable operations.

use crate::store::{MAX_PAGE_SIZE, Operation, OperationKind, SqliteStore, StoreError, WorkState};
use async_trait::async_trait;
use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};
use tokio::{
    sync::{Mutex, Semaphore},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

/// Sanitized failure returned while executing a durable operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum OperationError {
    /// The application state could not be loaded.
    #[error("application state is unavailable")]
    StateUnavailable,
    /// The durable operation journal could not be read or updated.
    #[error("operation journal is unavailable")]
    JournalUnavailable,
    /// Operation execution was cancelled.
    #[error("operation was cancelled")]
    Cancelled,
    /// A newer application generation made this operation obsolete.
    #[error("operation was superseded by a newer application generation")]
    Superseded,
    /// A runtime resource is not safely owned by the application.
    #[error("a Docker resource is not safely owned by this application")]
    OwnershipConflict,
    /// An immutable Docker resource differs from the desired configuration.
    #[error("Docker resource configuration cannot be reconciled safely in place")]
    DockerConfigurationConflict,
    /// Docker is not an active Swarm manager.
    #[error("Docker is not an active Swarm manager")]
    SwarmManagerUnavailable,
    /// The Docker Swarm topology is unsupported.
    #[error("Docker Swarm must contain exactly one manager node")]
    SwarmTopologyUnsupported,
    /// Docker Engine is unavailable while performing the described operation.
    #[error("Docker Engine is unavailable while {0}")]
    DockerUnavailable(&'static str),
    /// An image could not be resolved while performing the described operation.
    #[error("container image could not be resolved to a digest while {0}")]
    ImageResolutionFailed(&'static str),
    /// A Docker request failed while performing the described operation.
    #[error("Docker request failed while {0}")]
    DockerRequestFailed(&'static str),
    /// A local runtime value failed validation before a Docker request was made.
    #[error("Docker request validation failed while {0}")]
    ValidationFailed(&'static str),
    /// A service update failed in Docker.
    #[error("service update paused after task failure; the previous healthy task is retained")]
    ServiceUpdateFailed,
    /// The runtime plan is blocked by a diagnostic without a specific mapping.
    #[error("runtime plan is blocked by {0}")]
    PlanBlocked(&'static str),
    /// A service did not converge before its deadline.
    #[error("service did not converge before the deadline")]
    ConvergenceTimeout,
    /// Application-owned resources still exist when deletion reaches its final barrier.
    #[error("application deletion has not converged")]
    DeletionNotConverged,
}

impl OperationError {
    /// Returns the stable machine-readable failure code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::StateUnavailable => "state_unavailable",
            Self::JournalUnavailable => "journal_unavailable",
            Self::Cancelled => "cancelled",
            Self::Superseded => "superseded",
            Self::OwnershipConflict => "ownership_conflict",
            Self::DockerConfigurationConflict => "docker_configuration_conflict",
            Self::SwarmManagerUnavailable => "swarm_manager_unavailable",
            Self::SwarmTopologyUnsupported => "swarm_topology_unsupported",
            Self::DockerUnavailable(_) => "docker_unavailable",
            Self::ImageResolutionFailed(_) => "image_resolution_failed",
            Self::DockerRequestFailed(_) => "docker_request_failed",
            Self::ValidationFailed(_) => "validation_failed",
            Self::ServiceUpdateFailed => "service_update_failed",
            Self::PlanBlocked(_) => "plan_blocked",
            Self::ConvergenceTimeout => "convergence_timeout",
            Self::DeletionNotConverged => "deletion_not_converged",
        }
    }

    /// Formats the stable, sanitized public failure message.
    #[must_use]
    pub fn message(&self) -> String {
        format!("{self}")
    }

    /// Returns the stable code and sanitized message as a persistence-ready pair.
    #[must_use]
    pub fn tuple(&self) -> (&'static str, String) {
        (self.code(), self.message())
    }
}

#[cfg(test)]
mod operation_error_tests {
    use super::OperationError;

    #[test]
    fn operation_error_exposes_stable_sanitized_parts() {
        let error = OperationError::OwnershipConflict;
        assert_eq!(error.code(), "ownership_conflict");
        assert_eq!(
            error.message(),
            "a Docker resource is not safely owned by this application".to_owned()
        );
        assert_eq!(error.tuple(), (error.code(), error.message()));
    }
}

/// Executes one already-durable operation. Implementations must be idempotent:
/// startup recovery can invoke an interrupted operation again.
#[async_trait]
pub trait OperationHandler: Send + Sync + 'static {
    /// Performs operation work.
    async fn execute(
        &self,
        operation: &Operation,
        cancellation: &CancellationToken,
    ) -> Result<(), OperationError>;
}

/// Scheduler failures. Individual operation failures are journaled and are not scheduler failures.
#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    /// Durable journal access failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// An operation task unexpectedly panicked.
    #[error("operation task failed")]
    Task(#[source] tokio::task::JoinError),
    /// A scheduler semaphore was closed unexpectedly.
    #[error("operation scheduler is unavailable")]
    Semaphore(#[source] tokio::sync::AcquireError),
}

/// In-process dispatcher with global and per-application bounds.
pub struct OperationScheduler<H> {
    repository: Arc<SqliteStore>,
    handler: Arc<H>,
    global: Arc<Semaphore>,
    applications: Arc<Mutex<HashMap<String, Weak<Mutex<()>>>>>,
}

impl<H> OperationScheduler<H>
where
    H: OperationHandler,
{
    async fn execute_claimed(
        repository: &SqliteStore,
        handler: &H,
        operation_id: &str,
        token: &CancellationToken,
    ) -> Result<Result<(), OperationError>, StoreError> {
        let claimed = repository.operation(operation_id).await?;
        Ok(handler.execute(&claimed, token).await)
    }

    async fn finish_claimed(
        repository: &SqliteStore,
        operation: &Operation,
        result: Result<(), OperationError>,
    ) -> Result<(), StoreError> {
        match result {
            Ok(()) if operation.kind == OperationKind::Delete => {
                repository.finish_delete_operation(operation).await
            }
            Ok(()) => {
                repository
                    .transition_operation(
                        &operation.id,
                        WorkState::Running,
                        WorkState::Succeeded,
                        None,
                    )
                    .await
            }
            Err(OperationError::Cancelled | OperationError::Superseded) => {
                repository
                    .transition_operation(
                        &operation.id,
                        WorkState::Running,
                        WorkState::Cancelled,
                        None,
                    )
                    .await
            }
            Err(error) => {
                repository
                    .transition_operation(
                        &operation.id,
                        WorkState::Running,
                        WorkState::Failed,
                        Some(error),
                    )
                    .await
            }
        }
    }

    /// Creates a scheduler. The concurrency limit must be non-zero.
    ///
    /// # Panics
    /// Panics when the concurrency limit is zero.
    #[must_use]
    pub fn new(repository: Arc<SqliteStore>, handler: Arc<H>, max_operations: usize) -> Self {
        assert!(max_operations > 0, "scheduler limits must be positive");
        Self {
            repository,
            handler,
            global: Arc::new(Semaphore::new(max_operations)),
            applications: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Marks interrupted work recoverable and dispatches queued work until the current queue is empty.
    /// New operations arriving after the empty observation are handled by the next call/controller tick.
    ///
    /// # Errors
    /// Returns a sanitized repository error or an unexpected task failure.
    pub async fn recover_and_run(
        &self,
        cancellation: CancellationToken,
    ) -> Result<u64, SchedulerError> {
        let recovered = self.repository.recover_interrupted().await?;
        self.run_until_idle(cancellation).await?;
        Ok(recovered)
    }

    /// Drains a snapshot of pending/recovery operations while respecting all limits.
    ///
    /// # Errors
    /// Returns a sanitized repository error or an unexpected task failure.
    pub async fn run_until_idle(
        &self,
        cancellation: CancellationToken,
    ) -> Result<(), SchedulerError> {
        loop {
            if cancellation.is_cancelled() {
                return Ok(());
            }
            let operations = self.repository.pending_operations(MAX_PAGE_SIZE).await?;
            if operations.is_empty() {
                return Ok(());
            }
            let mut tasks = JoinSet::new();
            for operation in operations {
                let repository = Arc::clone(&self.repository);
                let handler = Arc::clone(&self.handler);
                let global = Arc::clone(&self.global);
                let applications = Arc::clone(&self.applications);
                let token = cancellation.child_token();
                tasks.spawn(async move {
                    let app_lock = {
                        let mut locks = applications.lock().await;
                        locks.retain(|_, lock| lock.strong_count() > 0);
                        let key = operation.application_id.to_string();
                        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
                            lock
                        } else {
                            let lock = Arc::new(Mutex::new(()));
                            locks.insert(key, Arc::downgrade(&lock));
                            lock
                        }
                    };
                    let _global = tokio::select! {
                        biased;
                        () = token.cancelled() => return Ok(()),
                        permit = global.acquire_owned() => permit.map_err(SchedulerError::Semaphore)?,
                    };
                    let _application = tokio::select! {
                        biased;
                        () = token.cancelled() => return Ok(()),
                        guard = app_lock.lock() => guard,
                    };
                    match repository
                        .transition_operation(
                            &operation.id,
                            operation.state,
                            WorkState::Running,
                            None,
                        )
                        .await
                    {
                        Ok(()) => {}
                        Err(StoreError::IllegalTransition) => return Ok(()),
                        Err(error) => return Err(error.into()),
                    }
                    let result = tokio::select! {
                        biased;
                        () = token.cancelled() => {
                            repository.transition_operation(
                                &operation.id,
                                WorkState::Running,
                                WorkState::Recovery,
                                None,
                            ).await?;
                            return Ok(());
                        },
                        result = Self::execute_claimed(&repository, &handler, &operation.id, &token) => result?,
                    };
                    Self::finish_claimed(&repository, &operation, result).await?;
                    Ok::<(), SchedulerError>(())
                });
            }
            let mut first_error = None;
            while let Some(result) = tasks.join_next().await {
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                    }
                    Err(error) => {
                        if first_error.is_none() {
                            first_error = Some(SchedulerError::Task(error));
                        }
                    }
                }
            }
            if let Some(error) = first_error {
                return Err(error);
            }
            // Every operation in the snapshot is now terminal or recovery due to cancellation.
            if cancellation.is_cancelled() {
                return Ok(());
            }
        }
    }
}
