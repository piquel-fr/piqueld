//! Docker-backed runtime preparation and durable reconciliation.

use crate::{
    api::{BoundaryError, PreparedApplication, RuntimeBoundary},
    docker::{DockerApi, DockerError},
    operations::{OperationError, OperationHandler, OperationScheduler, SchedulerError},
    store::{
        ApplicationRepository, ApplicationState, MAX_PAGE_SIZE, Operation, OperationKind,
        SqliteStore, StatusRepository, StepState, StoreError, StoredApplication,
    },
};
use async_trait::async_trait;
use piqueld_core::{
    InstanceId, NormalizedApplication, PlanAction, PlanRequest, ResolutionSet, compile_application,
    manifest::Source,
    plan,
    planner::ActionKind,
    resource::{Convergence, ResolvedSource},
};
use std::{sync::Arc, time::Duration};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const INGRESS_NETWORK: &str = "piqueld-ingress";

/// Executes durable operations against `Docker` and `SQLite`.
pub struct ReconcileHandler<D> {
    docker: Arc<D>,
    store: Arc<SqliteStore>,
    retry: RetryPolicy,
}

impl<D> ReconcileHandler<D> {
    /// Creates a handler with the default retry policy.
    #[must_use]
    pub fn new(docker: Arc<D>, store: Arc<SqliteStore>) -> Self {
        Self {
            docker,
            store,
            retry: RetryPolicy::default(),
        }
    }

    /// Replaces the retry policy used by this handler.
    #[must_use]
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }
}

#[derive(Clone, Copy)]
/// Retry and convergence timing for operation execution.
pub struct RetryPolicy {
    /// Maximum number of attempts for a retryable action.
    pub attempts: u32,
    /// Delay before the first retry.
    pub initial_delay: Duration,
    /// Upper bound for exponential retry delay.
    pub max_delay: Duration,
    /// Maximum time spent waiting for runtime convergence.
    pub convergence_timeout: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: 4,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(2),
            convergence_timeout: Duration::from_mins(2),
        }
    }
}

mod actions;
mod coordinator;
mod handler;
mod runtime;
#[cfg(test)]
mod tests;

pub use coordinator::{run_coordinator, run_event_hints};
pub use runtime::DockerRuntime;
