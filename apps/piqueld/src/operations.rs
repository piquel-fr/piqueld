//! Bounded, cancellation-aware scheduling for durable operations.

use crate::store::{Operation, OperationKind, OperationRepository, StoreError, WorkState};
use async_trait::async_trait;
use std::{collections::HashMap, sync::Arc};
use tokio::{
    sync::{Mutex, Semaphore},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

/// Executes one already-durable operation. Implementations must be idempotent:
/// startup recovery can invoke an interrupted operation again.
#[async_trait]
pub trait OperationHandler: Send + Sync + 'static {
    /// Performs operation work. Returned text must already be safe for durable/public storage.
    async fn execute(
        &self,
        operation: &Operation,
        cancellation: &CancellationToken,
    ) -> Result<(), (&'static str, &'static str)>;
}

/// Scheduler failures. Individual operation failures are journaled and are not scheduler failures.
#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    /// Durable journal access failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// An operation task unexpectedly panicked.
    #[error("operation task failed")]
    Task,
}

/// In-process dispatcher with global, build-specific, and per-application bounds.
pub struct OperationScheduler<R, H> {
    repository: Arc<R>,
    handler: Arc<H>,
    global: Arc<Semaphore>,
    builds: Arc<Semaphore>,
    applications: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl<R, H> OperationScheduler<R, H>
where
    R: OperationRepository + 'static,
    H: OperationHandler,
{
    async fn execute_claimed(
        repository: &R,
        handler: &H,
        operation_id: &str,
        token: &CancellationToken,
    ) -> Result<Result<(), (&'static str, &'static str)>, StoreError> {
        let claimed = repository.operation(operation_id).await?;
        Ok(handler.execute(&claimed, token).await)
    }

    /// Creates a scheduler. Both concurrency limits must be non-zero.
    ///
    /// # Panics
    /// Panics when either concurrency limit is zero.
    #[must_use]
    pub fn new(
        repository: Arc<R>,
        handler: Arc<H>,
        max_operations: usize,
        max_builds: usize,
    ) -> Self {
        assert!(
            max_operations > 0 && max_builds > 0,
            "scheduler limits must be positive"
        );
        Self {
            repository,
            handler,
            global: Arc::new(Semaphore::new(max_operations)),
            builds: Arc::new(Semaphore::new(max_builds)),
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
            let operations = self.repository.pending_operations(256).await?;
            if operations.is_empty() {
                return Ok(());
            }
            let mut tasks = JoinSet::new();
            for operation in operations {
                let repository = Arc::clone(&self.repository);
                let handler = Arc::clone(&self.handler);
                let global = Arc::clone(&self.global);
                let builds = Arc::clone(&self.builds);
                let applications = Arc::clone(&self.applications);
                let token = cancellation.child_token();
                tasks.spawn(async move {
                    let app_lock = {
                        let mut locks = applications.lock().await;
                        Arc::clone(
                            locks
                                .entry(operation.application_id.to_string())
                                .or_insert_with(|| Arc::new(Mutex::new(()))),
                        )
                    };
                    let _global = tokio::select! {
                        biased;
                        () = token.cancelled() => return Ok(()),
                        permit = global.acquire_owned() => permit.map_err(|_| StoreError::Database)?,
                    };
                    let _build = if operation.kind == OperationKind::Build {
                        Some(tokio::select! {
                            biased;
                            () = token.cancelled() => return Ok(()),
                            permit = builds.acquire_owned() => permit.map_err(|_| StoreError::Database)?,
                        })
                    } else {
                        None
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
                        Err(error) => return Err(error),
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
                    match result {
                        Ok(()) => repository
                            .transition_operation(
                                &operation.id,
                                WorkState::Running,
                                WorkState::Succeeded,
                                None,
                            )
                            .await?,
                        Err((code, message)) => repository
                            .transition_operation(
                                &operation.id,
                                WorkState::Running,
                                WorkState::Failed,
                                Some((code, message)),
                            )
                            .await?,
                    }
                    Ok::<(),StoreError>(())
                });
            }
            while let Some(result) = tasks.join_next().await {
                result.map_err(|_| SchedulerError::Task)??;
            }
            // Every operation in the snapshot is now terminal or recovery due to cancellation.
            if cancellation.is_cancelled() {
                return Ok(());
            }
        }
    }
}
