//! Shared daemon operations, independent of transport adapters.

mod exec;
mod history;
mod notification_worker;
mod observability;
mod previews;
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
    ApplicationId, EnvironmentId, EnvironmentName, GitBranch, PreviewSlot, TrackedBranch,
    api::{SecretAccess, SecretMetadata, StoredSecret},
    manifest::{ApplicationTemplate, ValidatedTemplate},
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
    /// The manifest repository could not be read, so no branch is known gone.
    #[error("the manifest repository could not be read")]
    RepositoryUnavailable(#[source] anyhow::Error),
}

pub use crate::store::Actor;

/// Validated application or environment mutation. Its serialization defines
/// request replay identity.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Mutation {
    /// Save configuration, optionally deploying its snapshot atomically.
    Save {
        /// Validated configuration, with references unresolved.
        application: Box<ApplicationTemplate>,
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
        /// Branch to follow instead of the one `spec.manifest` names.
        #[serde(skip_serializing_if = "Option::is_none")]
        branch: Option<TrackedBranch>,
    },
    /// Point an environment of a repository-backed application at another branch.
    SetBranch {
        /// Stable environment ID.
        id: EnvironmentId,
        /// Branch to follow, optionally pinned to a commit.
        branch: TrackedBranch,
    },
    /// Change only an environment's name.
    RenameEnvironment {
        /// Stable environment ID.
        id: EnvironmentId,
        /// New name, unique within the application.
        name: EnvironmentName,
    },
    /// Deploy an environment from its source: the application's saved
    /// configuration, or the manifest on its branch.
    Deploy {
        /// Stable environment identity.
        id: EnvironmentId,
        /// Fetch the manifest from this revision instead, for this deployment only.
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
    /// Change a preview.
    Preview(PreviewMutation),
}

/// A change to a preview. Previews select no `[spec.environments.<name>]`
/// block, so none needs or advances the application revision.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "preview", rename_all = "snake_case")]
pub enum PreviewMutation {
    /// Create and deploy a preview of a branch, or return the existing
    /// preview of that branch and slot without redeploying it.
    Create {
        /// Stable application ID.
        application: ApplicationId,
        /// Branch of the application's manifest repository.
        branch: GitBranch,
        /// Distinguishes several previews of one branch.
        #[serde(skip_serializing_if = "Option::is_none")]
        slot: Option<PreviewSlot>,
    },
    /// Deploy the head of a preview's branch.
    Deploy {
        /// Stable preview ID.
        id: EnvironmentId,
    },
    /// Delete a preview with every volume it created.
    Delete {
        /// Stable preview ID.
        id: EnvironmentId,
    },
    /// Delete a preview whose branch `repository` no longer has, unless the
    /// application's manifest repository changed since (`IdentityConflict`).
    Prune {
        /// Stable preview ID.
        id: EnvironmentId,
        /// URL of the repository the branch was found gone from.
        repository: String,
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
    /// Created or existing preview. Boxed, as the largest response, so
    /// acceptance futures stay small.
    Preview(Box<piqueld_core::api::CreatedPreview>),
    /// Accepted application deletion.
    Deleted(piqueld_core::api::DeletedApplication),
}

impl Mutation {
    /// Application permissions this mutation needs on its application (the
    /// environment's, for environment changes): `Write` and optionally
    /// `Deploy` for saves and edits. Previews need `Deploy` to create and
    /// deploy, and `Delete` to delete.
    #[must_use]
    pub fn required(&self) -> &'static [piqueld_core::access::AppPermission] {
        use piqueld_core::access::AppPermission::{Delete, Deploy, Write};
        match self {
            Self::Save { deploy: true, .. } | Self::Edit { deploy: true, .. } => &[Write, Deploy],
            Self::Save { .. }
            | Self::Edit { .. }
            | Self::Rename { .. }
            | Self::CreateEnvironment { .. }
            | Self::RenameEnvironment { .. }
            | Self::SetBranch { .. } => &[Write],
            Self::Deploy { .. }
            | Self::Reconcile { .. }
            | Self::Preview(PreviewMutation::Create { .. } | PreviewMutation::Deploy { .. }) => {
                &[Deploy]
            }
            Self::DeleteApplication { .. }
            | Self::Delete { .. }
            | Self::Preview(PreviewMutation::Delete { .. } | PreviewMutation::Prune { .. }) => {
                &[Delete]
            }
        }
    }

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
        manifest: ValidatedTemplate,
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
    fn pending_application(manifest: ValidatedTemplate) -> ApplicationTemplate {
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
    /// API requests refused since start, exported as a metric.
    denials: Arc<std::sync::atomic::AtomicU64>,
    /// Slots for audit records waiting to be written.
    audit_backlog: Arc<tokio::sync::Semaphore>,
    /// The share of `audit_backlog` anonymous records may hold.
    anonymous_backlog: Arc<tokio::sync::Semaphore>,
    /// Credential and network pairs already noted (see `observe_address`).
    known_addresses: Arc<observability::KnownAddresses>,
}

/// Audit records allowed to wait for the writer at once.
pub(crate) const AUDIT_BACKLOG: usize = 1024;

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
            denials: Arc::default(),
            audit_backlog: Arc::new(tokio::sync::Semaphore::new(AUDIT_BACKLOG)),
            anonymous_backlog: Arc::new(tokio::sync::Semaphore::new(AUDIT_BACKLOG / 2)),
            known_addresses: Arc::default(),
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
        actor: Actor<'_>,
    ) -> Result<piqueld_core::api::SecretKeyRecovery, ApplicationError> {
        Ok(self.store.recover_secret_key(actor).await?)
    }

    /// Lists an environment's generated secrets without exposing their values.
    ///
    /// # Errors
    /// Returns a storage error when the environment or its metadata cannot be read.
    pub async fn secrets(
        &self,
        environment: &EnvironmentId,
    ) -> Result<Vec<SecretMetadata>, ApplicationError> {
        Ok(self.store.secrets(environment).await?)
    }

    /// Lists an application's stored secrets and their access, never values.
    ///
    /// # Errors
    /// Returns a storage error when the application or its metadata cannot be read.
    pub async fn stored_secrets(
        &self,
        application: &ApplicationId,
    ) -> Result<Vec<StoredSecret>, ApplicationError> {
        Ok(self.store.stored_secrets(application).await?)
    }

    /// Stores a new version of an application's secret after checking the
    /// inspected generation; `access` replaces its access list.
    ///
    /// # Errors
    /// Returns a validation, generation conflict, or storage error.
    pub async fn put_stored_secret(
        &self,
        actor: Actor<'_>,
        application: &ApplicationId,
        name: &str,
        expected_generation: i64,
        value: Vec<u8>,
        access: Option<&SecretAccess>,
    ) -> Result<StoredSecret, ApplicationError> {
        Ok(self
            .store
            .put_stored_secret(actor, application, name, expected_generation, value, access)
            .await?)
    }

    /// Replaces the access list of an application's secret.
    ///
    /// # Errors
    /// Returns absence, unknown environments, or storage errors.
    pub async fn set_secret_access(
        &self,
        actor: Actor<'_>,
        application: &ApplicationId,
        name: &str,
        access: &SecretAccess,
    ) -> Result<StoredSecret, ApplicationError> {
        Ok(self
            .store
            .set_secret_access(actor, application, name, access)
            .await?)
    }

    /// Generates a new version of an environment's generated secret after
    /// checking the inspected generation; see `Store::regenerate_secret`.
    ///
    /// # Errors
    /// Returns absence, generation conflict, or storage errors.
    pub async fn regenerate_secret(
        &self,
        actor: Actor<'_>,
        environment: &EnvironmentId,
        name: &str,
        expected_generation: i64,
    ) -> Result<SecretMetadata, ApplicationError> {
        Ok(self
            .store
            .regenerate_secret(actor, environment, name, expected_generation)
            .await?)
    }

    /// Removes an unreferenced generated secret and all of its runtime versions.
    /// See `remove_secret_versions`.
    ///
    /// # Errors
    /// Returns when the secret is referenced or storage or runtime cleanup fails.
    pub async fn delete_secret(
        &self,
        actor: Actor<'_>,
        environment: &EnvironmentId,
        name: &str,
        expected_generation: i64,
    ) -> Result<(), ApplicationError> {
        let deletion = self
            .store
            .begin_secret_deletion(actor, environment, name, expected_generation)
            .await?;
        self.remove_secret_versions(actor, name, &deletion).await?;
        self.store
            .finish_secret_deletion(actor.attribution(), environment, name, &deletion.id)
            .await?;
        Ok(())
    }

    /// Removes an application's secret that no environment uses, and every
    /// environment's Docker secrets for it. See `remove_secret_versions`.
    ///
    /// # Errors
    /// Returns when the secret is referenced or storage or runtime cleanup fails.
    pub async fn delete_stored_secret(
        &self,
        actor: Actor<'_>,
        application: &ApplicationId,
        name: &str,
        expected_generation: i64,
    ) -> Result<(), ApplicationError> {
        let deletion = self
            .store
            .begin_stored_secret_deletion(actor, application, name, expected_generation)
            .await?;
        self.remove_secret_versions(actor, name, &deletion).await?;
        self.store
            .finish_stored_secret_deletion(actor.attribution(), application, name, &deletion.id)
            .await?;
        Ok(())
    }

    /// Removes the Docker secrets of a reserved deletion, environment by
    /// environment:
    ///
    /// 1. Journals a `remove_secrets` action by `actor` and removes the Swarm
    ///    secret versions.
    /// 2. Records the action outcome.
    ///
    /// A failed runtime cleanup leaves the reservation in place, so retrying the
    /// deletion resumes it (see `secret_deleting`).
    async fn remove_secret_versions(
        &self,
        actor: Actor<'_>,
        name: &str,
        deletion: &crate::store::SecretDeletion,
    ) -> Result<(), ApplicationError> {
        for (environment, versions) in &deletion.versions {
            let journal = self
                .store
                .begin_application_action(
                    actor.attribution(),
                    environment,
                    "remove_secrets",
                    Some(name),
                )
                .await?;
            let result = match self.store.action_request(&journal, 1).await {
                Ok(()) => self.runtime.remove_secrets(environment, versions).await,
                Err(error) => Err(error.into()),
            };
            self.store
                .finish_action(
                    &journal,
                    result.as_ref().err().map(BoundaryError::diagnostic),
                )
                .await?;
            result?;
        }
        Ok(())
    }

    /// Accepts a mutation from `actor` and records its receipt in the same
    /// transaction.
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
        actor: Actor<'_>,
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
                | Mutation::SetBranch { .. }
                | Mutation::Deploy { .. }
                | Mutation::Delete { .. } => expected_generation.is_none(),
                Mutation::Reconcile { .. } | Mutation::Preview(_) => false,
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
            .accept(actor, mutation, expected_generation, force, request_id)
            .await?;
        if wake {
            self.runtime.trigger_reconciliation();
        }
        Ok(response)
    }
}
