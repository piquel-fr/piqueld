//! Accept manifest intent immediately; Docker preparation belongs to operation execution.

mod runtime;
pub use runtime::DockerRuntime;

use crate::{
    docker::DockerError,
    store::{Store, StoreError, StoredApplication},
};
use async_trait::async_trait;
use piqueld_core::{
    ApplicationId, CompileError, NormalizedApplication, ObservedApplication, Operation,
    ValidatedApplication, resource::ResolvedApplication,
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
    /// Removes the supplied versions after logical-reference checks succeed.
    async fn remove_secrets(
        &self,
        _application: &ApplicationId,
        names: &[String],
    ) -> Result<(), BoundaryError> {
        if names.is_empty() {
            Ok(())
        } else {
            Err(DockerError::Unavailable("secret removal").into())
        }
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

/// Errors while accepting an application target.
#[derive(Debug, thiserror::Error)]
pub enum ApplicationError {
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
    /// Creates normalized apply intent before the store assigns application identity.
    /// # Panics
    /// Panics if the built-in placeholder ID is invalid.
    #[must_use]
    pub fn apply(manifest: ValidatedApplication, expected_application_id: Option<String>) -> Self {
        Self::Apply {
            application: manifest.normalize(
                ApplicationId::parse("pending-application").expect("valid placeholder ID"),
            ),
            expected_application_id,
        }
    }
}

/// Entry point for mutations from the HTTP API and direct callers.
///
/// This service validates request IDs and names, commits intent and the replay
/// receipt atomically through the store, then wakes reconciliation. Keeping this
/// here makes those callers share the same acceptance rules without requiring
/// the database layer to know about the controller.
#[derive(Clone)]
pub struct Applications {
    pub(crate) configuration: Option<piqueld_core::api::HostConfiguration>,
    pub(crate) store: Arc<Store>,
    pub(crate) runtime: Arc<dyn RuntimeBoundary>,
}

impl Applications {
    /// Creates the application service.
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
        self.configuration = Some(configuration);
        self
    }

    /// Accepts a mutation and records its receipt in the same transaction.
    /// `expected_generation` is the last inspected intent revision: zero requires
    /// absence and `None` skips the revision check. An explicit force override
    /// bypasses revision and name-based identity checks.
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

    /// Accepts normalized intent after checking Docker availability, without waiting for image preparation.
    /// # Errors
    /// Returns storage or generation errors.
    pub async fn apply(
        &self,
        manifest: ValidatedApplication,
        expected_generation: Option<u64>,
    ) -> Result<Operation, ApplicationError> {
        self.operation(Mutation::apply(manifest, None), expected_generation)
            .await
    }

    /// Requests deletion of the application's services and networks.
    /// # Errors
    /// Returns storage, absence, or generation errors.
    pub async fn delete(
        &self,
        id: &ApplicationId,
        expected_generation: Option<u64>,
    ) -> Result<Operation, ApplicationError> {
        self.operation(Mutation::Delete { id: id.clone() }, expected_generation)
            .await
    }

    /// Repairs latest intent using already prepared digests when available.
    /// # Errors
    /// Returns storage, absence, or generation errors.
    pub async fn reconcile(
        &self,
        id: &ApplicationId,
        expected_generation: Option<u64>,
    ) -> Result<Operation, ApplicationError> {
        self.operation(Mutation::Reconcile { id: id.clone() }, expected_generation)
            .await
    }

    /// Deploys saved configuration with fresh source resolution.
    /// # Errors
    /// Returns storage, deletion-intent, or generation errors.
    pub async fn deploy(
        &self,
        id: &ApplicationId,
        expected_generation: Option<u64>,
    ) -> Result<Operation, ApplicationError> {
        self.operation(Mutation::Deploy { id: id.clone() }, expected_generation)
            .await
    }

    async fn operation(
        &self,
        mutation: Mutation,
        expected_generation: Option<u64>,
    ) -> Result<Operation, ApplicationError> {
        let MutationResponse::Operation(accepted) = self
            .accept(mutation, expected_generation, false, None)
            .await?
        else {
            return Err(StoreError::Corrupt.into());
        };
        Ok(self.store.operation(&accepted.operation_id).await?)
    }
}
