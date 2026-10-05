//! Shared daemon operations, independent of transport adapters.

mod exec;
mod history;
mod notification_worker;
mod observability;
mod queries;
mod startup;
mod system;
mod views;

use crate::{
    application::{BoundaryError, RuntimeBoundary},
    store::{Store, StoreError},
};
pub use exec::ExecSession;
pub use history::ManifestExport;
use piqueld_core::{
    ApplicationId, EnvironmentId, EnvironmentName, NormalizedApplication, ValidatedApplication,
    api::SecretMetadata,
};
use std::sync::Arc;

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
    /// The requested service has no running task to execute a command in.
    #[error("service has no running task")]
    ServiceNotRunning,
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

/// Validated application or environment mutation. Its serialization defines
/// request replay identity.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Mutation {
    /// Save configuration, optionally deploying its snapshot atomically.
    Save {
        /// Normalized configuration.
        application: Box<NormalizedApplication>,
        /// Inspected application identity.
        expected_application_id: Option<String>,
        /// Whether to create a deployment of the only environment after saving.
        deploy: bool,
    },
    /// Edit saved configuration under the same revision check and transaction.
    Edit {
        /// Stable application identity.
        id: ApplicationId,
        /// Typed field or resource change.
        edit: Box<piqueld_core::edit::ApplicationEdit>,
        /// Capture a deployment of the only environment after saving.
        deploy: bool,
    },
    /// Change only the application's user-facing name.
    Rename {
        /// Stable application ID.
        id: ApplicationId,
        /// Validated new name.
        name: String,
    },
    /// Delete an application with all its environments.
    DeleteApplication {
        /// Stable application ID.
        id: ApplicationId,
        /// Names of every environment, required when there are several.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        environments: Vec<EnvironmentName>,
    },
    /// Add an environment to an application.
    CreateEnvironment {
        /// Stable application ID.
        application: ApplicationId,
        /// Name, unique within the application.
        name: EnvironmentName,
    },
    /// Change only an environment's name.
    RenameEnvironment {
        /// Stable environment ID.
        id: EnvironmentId,
        /// New name, unique within the application.
        name: EnvironmentName,
    },
    /// Deploy the application's latest saved configuration to an environment.
    Deploy {
        /// Stable environment identity.
        id: EnvironmentId,
        /// Fetch the manifest from this revision instead, without saving it.
        #[serde(skip_serializing_if = "Option::is_none")]
        revision: Option<piqueld_core::manifest::ManifestRevision>,
    },
    /// Request an environment's resource deletion.
    Delete {
        /// Stable environment ID.
        id: EnvironmentId,
    },
    /// Repair an environment's accepted intent.
    Reconcile {
        /// Stable environment ID.
        id: EnvironmentId,
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
    /// Created or renamed environment.
    Environment(piqueld_core::api::EnvironmentView),
    /// Accepted application deletion.
    Deleted(piqueld_core::api::DeletedApplication),
}

impl Mutation {
    /// Deploys saved configuration at its configured manifest revision.
    #[must_use]
    pub fn deploy(id: EnvironmentId) -> Self {
        Self::Deploy { id, revision: None }
    }

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
            application: Box::new(Self::pending_application(manifest)),
            expected_application_id,
            deploy,
        }
    }

    /// Normalizes under the `pending-application` placeholder ID; the store
    /// replaces it with the real (existing or newly minted) ID on acceptance.
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
    /// Managed ingress state, reported by readiness and used by plan previews.
    ingress: Option<Arc<crate::ingress::Ingress>>,
    tailnet: Option<tokio::sync::watch::Receiver<piqueld_core::api::TailnetStatus>>,
    /// Effective host settings exposed read-only; `None` unless attached at startup.
    configuration: Option<Arc<piqueld_core::api::HostConfiguration>>,
    store: Arc<Store>,
    /// Docker/Swarm adapter used for runtime requests and reconciliation wakeups.
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
            ingress: None,
            tailnet: None,
        }
    }

    /// Reports the tailnet node's latest status, when the daemon runs one.
    #[must_use]
    pub fn with_tailnet(
        mut self,
        status: Option<tokio::sync::watch::Receiver<piqueld_core::api::TailnetStatus>>,
    ) -> Self {
        self.tailnet = status;
        self
    }

    /// Shares managed ingress state with transport-independent daemon operations.
    #[must_use]
    pub fn with_ingress(mut self, ingress: Arc<crate::ingress::Ingress>) -> Self {
        self.ingress = Some(ingress);
        self
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

    /// Recovers from a lost master key by discarding every environment's stored values.
    /// # Errors
    /// Returns an error if the current key still works, or storage errors.
    pub async fn recover_secret_key(
        &self,
    ) -> Result<piqueld_core::api::SecretKeyRecovery, ApplicationError> {
        Ok(self.store.recover_secret_key().await?)
    }

    /// Lists secret metadata without exposing stored values.
    ///
    /// # Errors
    /// Returns a storage error when the environment or its metadata cannot be read.
    pub async fn secrets(
        &self,
        environment: &EnvironmentId,
    ) -> Result<Vec<SecretMetadata>, ApplicationError> {
        Ok(self.store.secrets(environment).await?)
    }

    /// Stores a new secret version after checking the inspected generation.
    ///
    /// # Errors
    /// Returns a validation, generation conflict, or storage error.
    pub async fn put_secret(
        &self,
        environment: &EnvironmentId,
        name: &str,
        expected_generation: i64,
        value: Vec<u8>,
    ) -> Result<SecretMetadata, ApplicationError> {
        Ok(self
            .store
            .put_secret(environment, name, expected_generation, value)
            .await?)
    }

    /// Removes an unreferenced secret and all of its runtime versions.
    ///
    /// 1. Reserves the secret for deletion (checking generation and references).
    /// 2. Journals a `remove_secrets` action and removes the Swarm secret versions.
    /// 3. Records the action outcome, then deletes the stored rows.
    ///
    /// A failed runtime cleanup leaves the reservation in place, so retrying the
    /// deletion resumes it (see `secret_deleting`).
    ///
    /// # Errors
    /// Returns when the secret is referenced or storage or runtime cleanup fails.
    pub async fn delete_secret(
        &self,
        environment: &EnvironmentId,
        name: &str,
        expected_generation: i64,
    ) -> Result<(), ApplicationError> {
        let deletion = self
            .store
            .begin_secret_deletion(environment, name, expected_generation)
            .await?;
        let journal = self
            .store
            .begin_application_action(environment, "remove_secrets", Some(name))
            .await?;
        let result = match self.store.action_request(&journal, 1).await {
            Ok(()) => {
                self.runtime
                    .remove_secrets(environment, &deletion.versions)
                    .await
            }
            Err(error) => Err(error.into()),
        };
        self.store
            .finish_action(
                &journal,
                result.as_ref().err().map(BoundaryError::diagnostic),
            )
            .await?;
        result?;
        self.store
            .finish_secret_deletion(environment, name, &deletion.id)
            .await?;
        Ok(())
    }

    /// Accepts a mutation and records its receipt in the same transaction.
    /// `expected_generation` is the last inspected intent revision: zero requires
    /// absence. Mutations other than reconcile require a revision; saves
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
                Mutation::Save {
                    expected_application_id,
                    ..
                } => {
                    expected_generation.is_none()
                        || (expected_generation != Some(0) && expected_application_id.is_none())
                }
                Mutation::Edit { .. }
                | Mutation::Rename { .. }
                | Mutation::DeleteApplication { .. }
                | Mutation::CreateEnvironment { .. }
                | Mutation::RenameEnvironment { .. }
                | Mutation::Deploy { .. }
                | Mutation::Delete { .. } => expected_generation.is_none(),
                Mutation::Reconcile { .. } => false,
            };
            if missing {
                return Err(ApplicationError::PreconditionRequired);
            }
        }
        // Request IDs are idempotency keys: 1–128 chars of `[A-Za-z0-9-_.:]`.
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
