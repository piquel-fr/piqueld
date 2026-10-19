//! Docker-backed runtime preparation and durable reconciliation.

use crate::{
    docker::{DockerApi, DockerError},
    operations::OperationError,
    store::{
        ApplicationState, Operation, OperationKind, OperationState, Store, StoreError,
        StoredEnvironment,
    },
};
use piqueld_core::{
    Plan, PlanRequest, codes,
    planner::ActionKind,
    resource::{APPLICATION_LABEL, Convergence},
};
use std::{sync::Arc, time::Duration};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Executes durable operations against `Docker` and `SQLite`.
pub struct Controller<D> {
    /// Docker client with concurrency limits shared by API observation and preview.
    docker: Arc<crate::docker::LimitedDocker<D>>,
    /// Serializes plan-action Docker mutations across applications (operation
    /// execution and active-target repair), so each journaled request is checked
    /// against the latest operation. Secret deletion takes it too, through
    /// `RuntimeBoundary::lock_mutations`.
    mutations: Arc<tokio::sync::Mutex<()>>,
    /// Deadline for resolving images, building sources, and fetching manifests.
    prepare_timeout: Duration,
    store: Arc<Store>,
    /// Managed ingress; without it, routes are staged and acknowledged directly.
    ingress: Option<Arc<crate::ingress::Ingress>>,
    retry: RetryPolicy,
    /// Image cleanup, and the lock preparations hold against it.
    images: images::ImageRetention,
    /// Asks the coordinator for a full scan now, so environments linked to
    /// one whose links changed replan within seconds rather than at the next
    /// scan interval, as do environments granted or revoked telemetry.
    relink: Arc<Notify>,
    /// Telemetry receivers' sockets; without them, none is mounted. Their
    /// directories are created and removed under `mutations`, so cleanup
    /// never removes one a service update is about to mount.
    sockets: Option<crate::telemetry::Sockets>,
}

impl<D> Controller<D> {
    /// Creates a controller with the default retry policy.
    #[must_use]
    pub fn new(docker: Arc<D>, store: Arc<Store>) -> Self {
        Self {
            docker: Arc::new(crate::docker::LimitedDocker::new(docker)),
            mutations: Arc::default(),
            prepare_timeout: Duration::from_mins(5),
            store,
            ingress: None,
            retry: RetryPolicy::default(),
            images: images::ImageRetention::default(),
            relink: Arc::new(Notify::new()),
            sockets: None,
        }
    }

    /// Mounts telemetry receivers' sockets from `sockets`.
    #[must_use]
    pub fn with_telemetry(mut self, sockets: Option<crate::telemetry::Sockets>) -> Self {
        self.sockets = sockets;
        self
    }

    /// Attaches installation-owned ingress without coupling its process lifetime.
    #[must_use]
    pub fn with_ingress(mut self, ingress: Arc<crate::ingress::Ingress>) -> Self {
        self.ingress = Some(ingress);
        self
    }

    /// Whether route intent should be projected onto runtime networks.
    fn ingress_enabled(&self) -> bool {
        self.ingress.as_ref().is_some_and(|ingress| ingress.enabled)
    }

    /// `target` as reconciliation plans it against `observed`: with the
    /// ingress networks its routes and the `accepted` ones need, the links
    /// it takes part in, and its telemetry socket while its environment is
    /// granted telemetry, whose directory the service update creates.
    /// Prepared targets never carry any of them.
    async fn runtime_target(
        &self,
        target: &piqueld_core::ResolvedApplication,
        accepted: &[piqueld_core::manifest::ValidatedRoute],
        observed: &piqueld_core::ObservedApplication,
    ) -> Result<piqueld_core::ResolvedApplication, StoreError> {
        let linked = self
            .store
            .with_links(
                target
                    .clone()
                    .with_ingress_routes(self.ingress_enabled(), accepted),
                observed,
            )
            .await?;
        let Some(sockets) = &self.sockets else {
            return Ok(linked);
        };
        let granted = self.store.telemetry_granted(&target.id).await?;
        Ok(sockets.project(linked, granted))
    }

    /// Applies the daemon's validated reconciliation timing configuration.
    #[must_use]
    pub fn with_config(mut self, config: &crate::config::ReconciliationConfig) -> Self {
        self.prepare_timeout = Duration::from_secs(config.prepare_timeout_seconds);
        self.retry.convergence_timeout = Duration::from_secs(config.convergence_timeout_seconds);
        self
    }

    /// Applies the daemon's image retention configuration.
    #[must_use]
    pub fn with_images(mut self, config: &crate::config::ImagesConfig) -> Self {
        self.images = images::ImageRetention::new(config);
        self
    }

    /// Sets the complete image-preparation deadline.
    #[must_use]
    pub fn with_prepare_timeout(mut self, timeout: Duration) -> Self {
        self.prepare_timeout = timeout;
        self
    }

    /// Replaces the retry policy used by this controller.
    ///
    /// # Panics
    /// Panics when the policy has no attempts or its initial delay exceeds its
    /// maximum delay.
    #[must_use]
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        if let Err(error) = retry.validate() {
            panic!("invalid retry policy: {error}");
        }
        self.retry = retry;
        self
    }

    /// The request removing every resource of environment `id`.
    fn delete_request(&self, id: &piqueld_core::EnvironmentId) -> PlanRequest {
        PlanRequest::Delete {
            environment_id: id.clone(),
            instance_id: self.store.instance_id().clone(),
        }
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
    /// Maximum time an operation waits without a service converging.
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
    #[must_use]
    pub fn runtime(&self, wake: Arc<Notify>) -> Arc<dyn crate::application::RuntimeBoundary> {
        Arc::new(
            crate::application::ApplicationRuntime::new(
                Arc::clone(&self.docker),
                &self.store,
                wake,
                self.prepare_timeout,
                self.images_lock(),
                Arc::clone(&self.mutations),
            )
            .with_rescan(Arc::clone(&self.relink)),
        )
    }
}

/// Why a plan is blocked: its first recognized blocking diagnostic, in
/// priority order.
#[derive(Clone, Copy)]
enum BlockedPlan {
    OwnershipConflict,
    ImmutableConfiguration,
    ServiceUpdateFailed,
    Unclassified,
}

impl BlockedPlan {
    /// Classifies a blocked `plan`: ownership first, then immutable
    /// configuration, then a failed service update.
    fn of(plan: &Plan) -> Self {
        let has = |code: &str| {
            plan.diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == code)
        };
        if has(codes::UNOWNED_NAME_COLLISION) {
            Self::OwnershipConflict
        } else if has(codes::IMMUTABLE_CONFIGURATION_DRIFT) {
            Self::ImmutableConfiguration
        } else if has(codes::SERVICE_UPDATE_FAILED) {
            Self::ServiceUpdateFailed
        } else {
            Self::Unclassified
        }
    }

    /// The error failing an operation whose plan is blocked.
    fn error(self) -> OperationError {
        match self {
            Self::OwnershipConflict => OperationError::OwnershipConflict,
            Self::ImmutableConfiguration => OperationError::DockerConfigurationConflict,
            Self::ServiceUpdateFailed => OperationError::ServiceUpdateFailed,
            Self::Unclassified => {
                OperationError::PlanBlocked("an unclassified blocking diagnostic")
            }
        }
    }

    /// The environment's status message.
    fn message(self) -> &'static str {
        match self {
            Self::OwnershipConflict => "runtime reconciliation is blocked by an ownership conflict",
            Self::ImmutableConfiguration => {
                "runtime reconciliation is blocked by immutable Docker configuration"
            }
            Self::ServiceUpdateFailed => {
                "runtime reconciliation is blocked by a failed service update"
            }
            Self::Unclassified => "runtime reconciliation is blocked",
        }
    }
}

mod actions;
mod controller;
mod coordinator;
mod deployment;
mod images;
pub use images::ImagesInUse;
mod jobs;
mod links;
