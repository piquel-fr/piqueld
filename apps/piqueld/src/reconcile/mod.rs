//! Docker-backed runtime preparation and durable reconciliation.

use crate::{
    docker::{DockerApi, DockerError},
    operations::OperationError,
    store::{
        ApplicationState, MAX_PAGE_SIZE, Operation, OperationKind, OperationState, SqliteStore,
        StoreError, StoredApplication,
    },
};
use piqueld_core::{
    Plan, PlanRequest, codes,
    planner::ActionKind,
    resource::{APPLICATION_LABEL, Convergence, INSTANCE_LABEL, MANAGED_LABEL},
};
use std::{sync::Arc, time::Duration};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Executes durable operations against `Docker` and `SQLite`.
pub struct Controller<D> {
    docker: Arc<crate::docker::LimitedDocker<D>>,
    mutations: tokio::sync::Mutex<()>,
    prepare_timeout: Duration,
    store: Arc<SqliteStore>,
    retry: RetryPolicy,
}

impl<D> Controller<D> {
    /// Creates a controller with the default retry policy.
    #[must_use]
    pub fn new(docker: Arc<D>, store: Arc<SqliteStore>) -> Self {
        Self {
            docker: Arc::new(crate::docker::LimitedDocker::new(docker)),
            mutations: tokio::sync::Mutex::new(()),
            prepare_timeout: Duration::from_secs(300),
            store,
            retry: RetryPolicy::default(),
        }
    }

    /// Sets the complete image-preparation deadline.
    #[must_use]
    pub fn with_prepare_timeout(mut self, timeout: Duration) -> Self {
        self.prepare_timeout = timeout;
        self
    }

    /// Replaces the retry policy used by this controller.
    #[must_use]
    ///
    /// # Panics
    /// Panics when the policy has no attempts or its initial delay exceeds its
    /// maximum delay.
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        if let Err(error) = retry.validate() {
            panic!("invalid retry policy: {error}");
        }
        self.retry = retry;
        self
    }
}

#[derive(Clone, Copy, Debug)]
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
/// Invalid retry-policy configuration.
pub enum RetryPolicyError {
    /// At least one operation attempt is required.
    #[error("retry policy attempts must be greater than zero")]
    ZeroAttempts,
    /// The first retry delay cannot exceed the configured maximum delay.
    #[error("retry policy initial_delay must not exceed max_delay")]
    InitialDelayExceedsMax,
}

impl RetryPolicy {
    /// Validates the policy before it is installed on a reconciler.
    ///
    /// # Errors
    /// Returns an error when no attempts are configured or the initial delay
    /// exceeds the maximum delay.
    pub fn validate(self) -> Result<(), RetryPolicyError> {
        if self.attempts == 0 {
            return Err(RetryPolicyError::ZeroAttempts);
        }
        if self.initial_delay > self.max_delay {
            return Err(RetryPolicyError::InitialDelayExceedsMax);
        }
        Ok(())
    }
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

impl<D: DockerApi> Controller<D> {
    /// Shares the controller's I/O limits with API observation and preview.
    /// # Panics
    /// Panics if the store violates its validated instance identity invariant.
    #[must_use]
    pub fn runtime(&self, wake: Arc<Notify>) -> Arc<dyn crate::application::RuntimeBoundary> {
        Arc::new(crate::application::DockerRuntime::new(
            Arc::clone(&self.docker),
            piqueld_core::InstanceId::parse(self.store.instance_id())
                .expect("store instance identity is valid"),
            wake,
            self.prepare_timeout,
        ))
    }
}

fn has_diagnostic(plan: &piqueld_core::Plan, code: &str) -> bool {
    plan.diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == code)
}

pub(super) fn blocked_plan_error(plan: &piqueld_core::Plan) -> OperationError {
    if has_diagnostic(plan, codes::UNOWNED_NAME_COLLISION) {
        OperationError::OwnershipConflict
    } else if has_diagnostic(plan, codes::IMMUTABLE_CONFIGURATION_DRIFT) {
        OperationError::DockerConfigurationConflict
    } else if has_diagnostic(plan, codes::SERVICE_UPDATE_FAILED) {
        OperationError::ServiceUpdateFailed
    } else {
        OperationError::PlanBlocked("an unclassified blocking diagnostic")
    }
}

pub(super) fn blocked_plan_message(plan: &piqueld_core::Plan) -> &'static str {
    if has_diagnostic(plan, codes::UNOWNED_NAME_COLLISION) {
        "runtime reconciliation is blocked by an ownership conflict"
    } else if has_diagnostic(plan, codes::IMMUTABLE_CONFIGURATION_DRIFT) {
        "runtime reconciliation is blocked by immutable Docker configuration"
    } else if has_diagnostic(plan, codes::SERVICE_UPDATE_FAILED) {
        "runtime reconciliation is blocked by a failed service update"
    } else {
        "runtime reconciliation is blocked"
    }
}

mod actions;
mod controller;
mod coordinator;
