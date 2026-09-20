//! Transport-independent daemon operations and runtime orchestration.

mod history;
mod queries;
mod startup;
mod system;
mod views;

mod runtime;
pub use history::ManifestExport;
pub use runtime::ApplicationRuntime;

use crate::{
    docker::DockerError,
    store::{Store, StoreError, StoredApplication},
};
use async_trait::async_trait;
use piqueld_core::{
    ApplicationId, CompileError, NormalizedApplication, ObservedApplication, ValidatedApplication,
    resource::ResolvedApplication,
};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
/// Errors crossing the runtime boundary.
pub enum BoundaryError {
    /// A Docker runtime request failed.
    #[error("runtime request failed")]
    Runtime(#[from] DockerError),
    /// Git checkout or image building failed, retaining internal diagnostics.
    #[error("Git source build failed")]
    GitBuild(#[source] anyhow::Error),
    /// Progress persistence failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Resolved inputs could not be compiled into desired runtime resources.
    #[error("application compilation failed")]
    Compilation(Vec<CompileError>),
}

/// Resolves accepted intent during execution and observes runtime for API reads.
#[async_trait]
pub trait RuntimeBoundary: Send + Sync + 'static {
    /// Reads recent workload logs from Docker.
    async fn logs(
        &self,
        id: &ApplicationId,
        service: Option<&str>,
        tail: u16,
        since: u32,
        stream: Option<piqueld_core::api::LogStream>,
    ) -> Result<piqueld_core::api::ApplicationLogs, BoundaryError>;

    /// Returns separate Engine and Swarm probe results. Unknown adapters fail closed.
    async fn readiness(&self) -> (bool, bool) {
        (false, false)
    }

    /// Wakes the reconciler after a mutation requests an immediate scan.
    fn trigger_reconciliation(&self) {}
    /// Resolves all mutable inputs into a complete immutable target.
    async fn prepare(
        &self,
        application: &NormalizedApplication,
        resolutions: &piqueld_core::ResolutionSet,
    ) -> Result<ResolvedApplication, BoundaryError>;
    /// Checks Docker availability without preparing images or changing runtime resources.
    async fn check_available(&self) -> Result<(), BoundaryError>;
    /// Captures current runtime state for a stored application.
    async fn observe(
        &self,
        application: &StoredApplication,
    ) -> Result<ObservedApplication, BoundaryError>;
}

/// Errors returned by transport-independent daemon operations.
#[derive(Debug, thiserror::Error)]
pub enum ApplicationError {
    /// A mutation needs an inspected revision and identity or an explicit force override.
    #[error("mutation preconditions are required")]
    PreconditionRequired,
    /// Application pagination is outside its supported bounds or has an invalid cursor.
    #[error("pagination parameters are invalid")]
    InvalidPagination,
    /// Workload log bounds or service filter are invalid.
    #[error("invalid log query")]
    InvalidLogQuery,
    /// No effective host configuration was attached to this service.
    #[error("effective host configuration is unavailable")]
    ConfigurationUnavailable,
    /// Saved configuration could not be rendered as TOML.
    #[error("could not render saved configuration")]
    ManifestSerialization(#[source] toml::ser::Error),
    /// Persistence failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The runtime plan contains a known conflict.
    #[error("runtime plan contains blocking conflicts")]
    PlanBlocked(Vec<piqueld_core::PlanDiagnostic>),
    /// Image resolution or compilation failed.
    #[error(transparent)]
    Runtime(#[from] BoundaryError),
}

/// Validated application mutation. Its serialization defines request replay identity.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Mutation {
    /// Replace intent by name, optionally requiring the previously inspected identity.
    Apply {
        /// Normalized manifest; acceptance assigns its stable ID.
        application: NormalizedApplication,
        /// Previously inspected stable ID.
        expected_application_id: Option<String>,
    },
    /// Save configuration, optionally deploying its snapshot atomically.
    Save {
        /// Normalized configuration.
        application: NormalizedApplication,
        /// Inspected application identity.
        expected_application_id: Option<String>,
        /// Whether to create a deployment after saving.
        deploy: bool,
    },
    /// Deploy the latest saved configuration.
    Deploy {
        /// Stable application identity.
        id: ApplicationId,
    },
    /// Request resource deletion.
    Delete {
        /// Stable application ID.
        id: ApplicationId,
    },
    /// Repair accepted intent.
    Reconcile {
        /// Stable application ID.
        id: ApplicationId,
    },
    /// Change only the user-facing name.
    Rename {
        /// Stable application ID.
        id: ApplicationId,
        /// Validated new name.
        name: String,
    },
}

/// Small acceptance response stored for request replay, without manifest contents.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum MutationResponse {
    /// Configuration persisted without implicit deployment.
    Saved(piqueld_core::api::SavedApplication),
    /// Accepted runtime operation.
    Operation(piqueld_core::api::AcceptedOperation),
    /// Completed metadata mutation.
    Rename(piqueld_core::api::RenamedApplication),
}

impl Mutation {
    /// Creates save intent, optionally deploying the saved snapshot atomically.
    /// # Panics
    /// Panics if the built-in placeholder ID is invalid.
    #[must_use]
    pub fn save(
        manifest: ValidatedApplication,
        expected_application_id: Option<String>,
        deploy: bool,
    ) -> Self {
        Self::Save {
            application: Self::pending_application(manifest),
            expected_application_id,
            deploy,
        }
    }

    /// Creates normalized apply intent before the store assigns application identity.
    /// # Panics
    /// Panics if the built-in placeholder ID is invalid.
    #[must_use]
    pub fn apply(manifest: ValidatedApplication, expected_application_id: Option<String>) -> Self {
        Self::Apply {
            application: Self::pending_application(manifest),
            expected_application_id,
        }
    }

    fn pending_application(manifest: ValidatedApplication) -> NormalizedApplication {
        manifest
            .normalize(ApplicationId::parse("pending-application").expect("valid placeholder ID"))
    }
}

/// Cheaply clonable entry point for all daemon operations.
///
/// HTTP, MCP, and scheduled jobs share this service; adapters only decode inputs
/// and encode results. Runtime and persistence stay private to this layer.
///
/// This service validates request IDs and names, commits intent and the replay
/// receipt atomically through the store, then wakes reconciliation. Keeping this
/// here makes those callers share the same acceptance rules without requiring
/// the database layer to know about the controller.
#[derive(Clone)]
pub struct ApplicationService {
    configuration: Option<Arc<piqueld_core::api::HostConfiguration>>,
    store: Arc<Store>,
    runtime: Arc<dyn RuntimeBoundary>,
}

impl ApplicationService {
    /// Creates a service over supplied storage and runtime adapters.
    /// No background work is started; use [`Self::start`] for daemon startup.
    #[must_use]
    pub fn new(store: Arc<Store>, runtime: Arc<dyn RuntimeBoundary>) -> Self {
        Self {
            store,
            runtime,
            configuration: None,
        }
    }

    /// Attaches the effective host configuration for read-only API inspection.
    #[must_use]
    pub fn with_configuration(
        mut self,
        configuration: piqueld_core::api::HostConfiguration,
    ) -> Self {
        self.configuration = Some(Arc::new(configuration));
        self
    }

    /// Accepts a mutation and records its receipt in the same transaction.
    /// `expected_generation` is the last inspected intent revision: zero requires
    /// absence. Mutations other than reconcile require a revision; apply and save
    /// also require the inspected identity when the revision is nonzero. An
    /// explicit force override bypasses revision and name-based identity checks.
    /// `request_id` is the caller's idempotency key, not an operation ID: replay
    /// returns the original response, including for rename which has no operation.
    /// # Errors
    /// Returns validation, conflict, or persistence errors.
    pub async fn accept(
        &self,
        mutation: Mutation,
        expected_generation: Option<u64>,
        force: bool,
        request_id: Option<&str>,
    ) -> Result<MutationResponse, ApplicationError> {
        if !force {
            let missing = match &mutation {
                Mutation::Apply {
                    expected_application_id,
                    ..
                }
                | Mutation::Save {
                    expected_application_id,
                    ..
                } => {
                    expected_generation.is_none()
                        || (expected_generation != Some(0) && expected_application_id.is_none())
                }
                Mutation::Deploy { .. } | Mutation::Delete { .. } | Mutation::Rename { .. } => {
                    expected_generation.is_none()
                }
                Mutation::Reconcile { .. } => false,
            };
            if missing {
                return Err(ApplicationError::PreconditionRequired);
            }
        }
        if request_id.is_some_and(|id| {
            id.is_empty()
                || id.len() > 128
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
        }) {
            return Err(StoreError::InvalidInput.into());
        }
        if let Mutation::Rename { name, .. } = &mutation
            && !piqueld_core::valid_logical_name(name)
        {
            return Err(StoreError::InvalidInput.into());
        }
        let (response, wake) = self
            .store
            .accept(mutation, expected_generation, force, request_id)
            .await?;
        if wake {
            self.runtime.trigger_reconciliation();
        }
        Ok(response)
    }
}
