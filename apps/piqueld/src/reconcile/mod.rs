//! Docker-backed runtime preparation and durable reconciliation.

use crate::{
    api::{BoundaryError, PreparedApplication, RuntimeBoundary},
    docker::{DockerApi, DockerError},
    operations::{OperationError, OperationHandler, OperationScheduler, SchedulerError},
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
    retry: RetryPolicy,
}

impl<D> ReconcileHandler<D> {
    /// Creates a handler with the default retry policy.
    
    ///
    
    /// # Examples
    
    ///
    
    /// ```no_run
    
    /// let handler = ReconcileHandler::new(docker, store);
    
    /// ```
    
    ///
    
    /// The `docker` and `store` values must be shared using `Arc`.
    
    #[must_use]
    pub fn new(docker: Arc<D>, store: Arc<SqliteStore>) -> Self {
        Self {
            docker,
            store,
            retry: RetryPolicy::default(),
        }
    }

    /// Replaces the retry policy used by this handler.
    ///
    /// # Panics
    ///
    /// Panics if the policy allows zero attempts or its initial delay exceeds its
    /// maximum delay.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// let handler = handler.with_retry_policy(RetryPolicy::default());
    /// ```
    #[must_use]
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
    /// Checks whether the retry policy has a valid configuration.
    ///
    /// # Errors
    ///
    /// Returns [`RetryPolicyError::ZeroAttempts`] when no attempts are configured,
    /// or [`RetryPolicyError::InitialDelayExceedsMax`] when the initial delay is
    /// greater than the maximum delay.
    ///
    /// # Examples
    ///
    /// ```
    /// let policy = RetryPolicy::default();
    /// assert!(policy.validate().is_ok());
    /// ```
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
    /// Provides the default retry policy for reconciliation operations.
    ///
    /// # Examples
    ///
    /// ```
    /// let policy = RetryPolicy::default();
    /// assert_eq!(policy.attempts, 4);
    /// ```
    fn default() -> Self {
        Self {
            attempts: 4,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(2),
            convergence_timeout: Duration::from_mins(2),
        }
    }
}

/// Checks whether a plan contains a diagnostic with the specified code.
///
/// # Examples
///
/// ```rust,ignore
/// let plan = /* a plan containing diagnostics */;
/// assert!(has_diagnostic(&plan, "EXAMPLE_CODE"));
/// ```
///
/// # Returns
///
/// `true` if the plan contains a diagnostic with the specified code, `false` otherwise.
fn has_diagnostic(plan: &piqueld_core::Plan, code: &str) -> bool {
    plan.diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == code)
}

/// Maps a blocked plan's diagnostics to the corresponding operation error.
///
/// Ownership-related diagnostics and unrecognized blocked-plan diagnostics map to
/// [`OperationError::OwnershipConflict`]. Immutable Docker configuration drift
/// maps to [`OperationError::DockerConfigurationConflict`].
///
/// # Examples
///
/// ```ignore
/// let error = blocked_plan_error(&plan);
/// assert_eq!(error, OperationError::OwnershipConflict);
/// ```
pub(super) fn blocked_plan_error(plan: &piqueld_core::Plan) -> OperationError {
    if has_diagnostic(plan, "unowned_name_collision") {
        OperationError::OwnershipConflict
    } else if has_diagnostic(plan, "immutable_configuration_drift") {
        OperationError::DockerConfigurationConflict
    } else {
        OperationError::OwnershipConflict
    }
}

/// Describes why runtime reconciliation is blocked by a plan diagnostic.
///
/// # Examples
///
/// ```rust,ignore
/// let message = blocked_plan_message(&plan);
/// assert_eq!(
///     message,
///     "runtime reconciliation is blocked by an ownership conflict"
/// );
/// ```
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
