//! Docker-backed runtime preparation and durable reconciliation.

use crate::{
    api::{BoundaryError, PreparedApplication, RuntimeBoundary},
    docker::{DockerApi, DockerError},
    operations::{OperationError, OperationHandler, OperationScheduler, SchedulerError},
    secrets::SecretService,
    store::{
        ApplicationState, MAX_PAGE_SIZE, Operation, OperationKind, SqliteStore, StepState,
        StoreError, StoredApplication,
    },
};
use piqueld_core::{
    InstanceId, NormalizedApplication, Plan, PlanAction, PlanRequest, ResolutionSet,
    compile_application,
    manifest::Source,
    planner::ActionKind,
    resource::{APPLICATION_LABEL, Convergence, INSTANCE_LABEL, MANAGED_LABEL, ResolvedSource},
};
use std::{sync::Arc, time::Duration};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Executes durable operations against `Docker` and `SQLite`.
pub struct ReconcileHandler<D> {
    docker: Arc<D>,
    store: Arc<SqliteStore>,
    secret_service: Option<Arc<SecretService>>,
    retry: RetryPolicy,
}

impl<D> ReconcileHandler<D> {
    /// Creates a handler with the default retry policy.
    #[must_use]
    pub fn new(docker: Arc<D>, store: Arc<SqliteStore>) -> Self {
        Self {
            docker,
            store,
            secret_service: None,
            retry: RetryPolicy::default(),
        }
    }

    /// Replaces the retry policy used by this handler.
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

    /// Enables encrypted logical-secret delivery for runtime actions.
    #[must_use]
    pub fn with_secret_service(mut self, service: Arc<SecretService>) -> Self {
        self.secret_service = Some(service);
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

fn has_diagnostic(plan: &piqueld_core::Plan, code: &str) -> bool {
    plan.diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == code)
}

pub(super) fn blocked_plan_error(plan: &piqueld_core::Plan) -> OperationError {
    if has_diagnostic(plan, "unowned_name_collision") {
        OperationError::OwnershipConflict
    } else if has_diagnostic(plan, "immutable_configuration_drift") {
        OperationError::DockerConfigurationConflict
    } else {
        OperationError::OwnershipConflict
    }
}

pub(super) fn blocked_plan_message(plan: &piqueld_core::Plan) -> &'static str {
    if has_diagnostic(plan, "unowned_name_collision") {
        "runtime reconciliation is blocked by an ownership conflict"
    } else if has_diagnostic(plan, "immutable_configuration_drift") {
        "runtime reconciliation is blocked by immutable Docker configuration"
    } else {
        "runtime reconciliation is blocked by an ownership conflict"
    }
}

mod actions;
mod coordinator;
mod handler;
mod runtime;
pub use coordinator::run_coordinator;
pub use runtime::DockerRuntime;
