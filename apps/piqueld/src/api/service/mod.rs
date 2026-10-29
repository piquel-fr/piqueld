//! Shared daemon operations, independent of transport adapters.

mod admission;
mod checks;
mod exec;
mod history;
mod notification_message;
mod notification_worker;
mod observability;
mod previews;
mod promotions;
mod queries;
mod startup;
mod sync;
mod system;
mod telemetry;
mod views;
mod webhooks;

use crate::store::Promotion;
use crate::{
    application::{BoundaryError, RuntimeBoundary},
    store::{Store, StoreError},
};
pub use admission::Expensive;
pub use exec::ExecSession;
pub use history::ManifestExport;
use piqueld_core::{
    ApplicationId, EnvironmentId, EnvironmentName, GitBranch, PreviewSlot, TrackedBranch,
    access::Grants,
    api::{LocalSnapshot, PromotionSelection, SecretAccess, SecretMetadata, StoredSecret},
    manifest::{ApplicationTemplate, RepositoryManifest, ValidatedTemplate},
};
use std::sync::Arc;
pub use webhooks::{Delivery, WEBHOOK_BODY_LIMIT, WEBHOOK_PATH, WebhookError};

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
    /// An uploaded local snapshot was refused.
    #[error(transparent)]
    Snapshot(#[from] crate::git::snapshot::SnapshotError),
    /// As much of this kind of expensive work already runs as it may.
    #[error("too much {0:?} work is running")]
    Busy(Expensive),
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
        /// Where it deploys from instead of the saved manifest or the branch
        /// `spec.manifest` names.
        #[serde(flatten)]
        source: Option<SourceChoice>,
    },
    /// Point an environment of a repository-backed application at another branch.
    SetBranch {
        /// Stable environment ID.
        id: EnvironmentId,
        /// Branch to follow, optionally pinned to a commit.
        branch: TrackedBranch,
    },
    /// Change where an environment's releases come from, or promote one.
    Promotion(PromotionMutation),
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
    /// Opt an environment in or out of its application's sync. Needs no
    /// application revision, since it changes nothing an environment deploys.
    SetSync {
        /// Stable environment ID.
        id: EnvironmentId,
        /// Whether pushes may deploy it.
        enabled: bool,
    },
    /// Deploy a branch head sync listed, unless sync already acted on it
    /// (see `Store::sync_on`). Needs no application revision.
    Sync(ListedHead),
    /// Grant or revoke an environment's telemetry, which its receiving
    /// service then gets from every application. Needs no application
    /// revision, since it changes nothing the environment deploys, but
    /// `telemetry:grant` (see [`Self::required_installation`]).
    SetTelemetry {
        /// Stable environment ID.
        id: EnvironmentId,
        /// Whether its receiving service gets telemetry.
        granted: bool,
    },
    /// Change a preview.
    Preview(PreviewMutation),
}

/// A branch head sync listed for an environment or preview.
#[derive(Debug, serde::Serialize)]
pub struct ListedHead {
    /// Stable environment or preview ID.
    pub id: EnvironmentId,
    /// URL of the repository that was listed.
    pub repository: String,
    /// Branch that was listed.
    pub branch: GitBranch,
    /// Full commit hash at its head, as listed by piqueld itself.
    pub head: String,
    /// The head of its last deployment, as recorded when the listing
    /// started, so a deployment that finished meanwhile, even of the same
    /// commit, is never undone with what was listed before it.
    pub since: Option<piqueld_core::sync::SyncedHead>,
}

/// What a [`Mutation::Sync`] did.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum SyncOutcome {
    /// Deployed the head.
    Deployed(piqueld_core::api::AcceptedOperation),
    /// Nothing to deploy: sync already acted on the head, its deployment
    /// already fetched it, or it no longer follows the branch.
    Current,
    /// A deployment is running; sync tries again once it finished.
    Busy,
}

/// Where a new environment deploys from, when not from the saved manifest
/// or the branch `spec.manifest` names. Serialized as the request field it
/// came from, so requests from before promotion keep their identity.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceChoice {
    /// Follow this branch of the manifest repository.
    Branch(TrackedBranch),
    /// Receive releases promoted from this environment.
    PromoteFrom(EnvironmentId),
}

/// A change to a promoted environment, or making one.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "promotion", rename_all = "snake_case")]
pub enum PromotionMutation {
    /// Make an environment promoted from another, or track its own source again.
    SetSource {
        /// Stable environment ID.
        id: EnvironmentId,
        /// The environment to receive releases from; `None` to track the
        /// saved manifest or the branch `spec.manifest` names again.
        promote_from: Option<EnvironmentId>,
    },
    /// Deploy a release into a promoted environment.
    Promote {
        /// Stable environment ID.
        id: EnvironmentId,
        /// What was requested; this, not the release it pinned, identifies
        /// the request for replay.
        selection: PromotionSelection,
        /// The release pinned before acceptance, checked again inside it;
        /// `None` only to look the request up for replay, which never
        /// executes it.
        #[serde(skip)]
        promotion: Option<Promotion>,
    },
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
    /// Deploy a snapshot of a local worktree of `branch`, already received
    /// (see `Snapshots`), to the preview of `branch` and `slot`, creating it
    /// first as `Create` does. Refused with `IdentityConflict` once the
    /// application reads another repository than `repository`, which the
    /// snapshot's base was fetched from.
    Local {
        /// Stable application ID.
        application: ApplicationId,
        /// Branch of the application's manifest repository.
        branch: GitBranch,
        /// Distinguishes several previews of one branch.
        #[serde(skip_serializing_if = "Option::is_none")]
        slot: Option<PreviewSlot>,
        /// URL of the repository the snapshot was received for.
        repository: String,
        /// The snapshot.
        snapshot: LocalSnapshot,
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
    /// Accepted promotion, with the release it pinned.
    Promotion(piqueld_core::api::AcceptedPromotion),
    /// Created or existing preview. Boxed, as the largest response, so
    /// acceptance futures stay small.
    Preview(Box<piqueld_core::api::CreatedPreview>),
    /// Accepted application deletion.
    Deleted(piqueld_core::api::DeletedApplication),
    /// What sync did with a branch head.
    Synced(SyncOutcome),
}

impl Mutation {
    /// Application permissions this mutation needs on its application (the
    /// environment's, for environment changes), whose repository connection
    /// is currently `connection`: `Write`, and `Deploy` too for saves and
    /// edits that deploy, and for changes sync then deploys (see
    /// [`Self::deploys_on_push`]). Previews need `previews:deploy` to create
    /// and deploy, and `previews:delete` to delete, which `Deploy` and
    /// `Delete` include.
    #[must_use]
    pub fn required(
        &self,
        connection: Option<&RepositoryManifest>,
    ) -> &'static [piqueld_core::access::AppPermission] {
        use piqueld_core::access::AppPermission::{
            Delete, Deploy, PreviewsDelete, PreviewsDeploy, Read, Write,
        };
        match self {
            Self::Save { deploy: true, .. } | Self::Edit { deploy: true, .. } => &[Write, Deploy],
            _ if self.deploys_on_push(connection) => &[Write, Deploy],
            Self::Save { .. }
            | Self::Edit { .. }
            | Self::Rename { .. }
            | Self::CreateEnvironment { .. }
            | Self::RenameEnvironment { .. }
            | Self::SetBranch { .. }
            | Self::SetSync { .. }
            | Self::Promotion(PromotionMutation::SetSource { .. }) => &[Write],
            Self::Deploy { .. }
            | Self::Promotion(PromotionMutation::Promote { .. })
            | Self::Reconcile { .. }
            | Self::Sync(_) => &[Deploy],
            Self::Preview(
                PreviewMutation::Create { .. }
                | PreviewMutation::Deploy { .. }
                | PreviewMutation::Local { .. },
            ) => &[PreviewsDeploy],
            Self::DeleteApplication { .. } | Self::Delete { .. } => &[Delete],
            Self::SetTelemetry { .. } => &[Read],
            Self::Preview(PreviewMutation::Delete { .. } | PreviewMutation::Prune { .. }) => {
                &[PreviewsDelete]
            }
        }
    }

    /// The installation-wide permission this mutation needs besides
    /// [`Self::required`]: granting telemetry reaches every application's.
    #[must_use]
    pub const fn required_installation(&self) -> Option<piqueld_core::access::GlobalPermission> {
        match self {
            Self::SetTelemetry { .. } => {
                Some(piqueld_core::access::GlobalPermission::TelemetryGrant)
            }
            _ => None,
        }
    }

    /// Whether accepting this mutation may change which environments
    /// receive telemetry: granting or revoking it, or deleting environments.
    #[must_use]
    pub const fn changes_receivers(&self) -> bool {
        matches!(
            self,
            Self::SetTelemetry { .. } | Self::Delete { .. } | Self::DeleteApplication { .. }
        )
    }

    /// Whether this change lets sync deploy what no deployment chose, given
    /// the application's current `connection`: turning sync on, which lets
    /// it deploy previews and opted-in environments whose branch moved since
    /// their last deployment, or opting an environment in while it syncs,
    /// including by saving a manifest that declares one with `sync = true`.
    /// Those take the permission to deploy. Nothing else does: sync follows
    /// only from a deployment's head, and a new environment, a new branch, or
    /// a new repository follows only after its next deployment. Changes to
    /// how a following environment renders, such as a rename, take effect
    /// with its next deployment, as without sync.
    #[must_use]
    pub fn deploys_on_push(&self, connection: Option<&RepositoryManifest>) -> bool {
        match self {
            Self::Save { application, .. } => application.spec().deploys_on_push(connection),
            Self::Edit { edit, .. } => edit.deploys_on_push(connection),
            Self::SetSync { enabled, .. } => {
                *enabled && connection.is_some_and(RepositoryManifest::syncs)
            }
            _ => false,
        }
    }

    /// Checks what submitting this change needs before any state is read or
    /// work is done for it: the revision precondition unless `force`d (see
    /// `lacks_precondition`), a well-formed idempotency key `request_id`, and
    /// a valid name for a rename.
    ///
    /// # Errors
    /// `PreconditionRequired`, or invalid input.
    pub fn check_submission(
        &self,
        expected_generation: Option<u64>,
        force: bool,
        request_id: Option<&str>,
    ) -> Result<(), ApplicationError> {
        if !force && self.lacks_precondition(expected_generation) {
            return Err(ApplicationError::PreconditionRequired);
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
        if let Self::Rename { name, .. } = self
            && !piqueld_core::valid_logical_name(name)
        {
            return Err(StoreError::InvalidInput.into());
        }
        Ok(())
    }

    /// Whether submitting this change without `force` lacks the revision
    /// precondition it needs: every change but reconcile, sync, telemetry
    /// grants and previews needs the inspected revision, and saves over a nonzero one also the
    /// inspected identity.
    const fn lacks_precondition(&self, expected_generation: Option<u64>) -> bool {
        match self {
            Self::Save {
                expected_application_id,
                ..
            } => {
                expected_generation.is_none()
                    || (!matches!(expected_generation, Some(0))
                        && expected_application_id.is_none())
            }
            Self::Edit { .. }
            | Self::Rename { .. }
            | Self::DeleteApplication { .. }
            | Self::CreateEnvironment { .. }
            | Self::RenameEnvironment { .. }
            | Self::SetBranch { .. }
            | Self::Promotion(_)
            | Self::Deploy { .. }
            | Self::Delete { .. } => expected_generation.is_none(),
            Self::Reconcile { .. }
            | Self::SetSync { .. }
            | Self::SetTelemetry { .. }
            | Self::Sync(_)
            | Self::Preview(_) => false,
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
    /// The built images as of the last cleanup; `None` without a controller.
    images: Option<tokio::sync::watch::Receiver<piqueld_core::api::ImageStatus>>,
    /// Effective host settings; `None` unless attached at startup.
    configuration: Option<Arc<Configuration>>,
    store: Arc<Store>,
    /// Docker/Swarm adapter used for runtime requests and reconciliation wakeups.
    runtime: Arc<dyn RuntimeBoundary>,
    /// Export of the daemon's traces, metrics and logs; disabled by default.
    telemetry: crate::telemetry::Telemetry,
    /// Slots for audit records waiting to be written.
    audit_backlog: Arc<tokio::sync::Semaphore>,
    /// The share of `audit_backlog` anonymous records may hold.
    anonymous_backlog: Arc<tokio::sync::Semaphore>,
    /// Audit records dropped or not written since the last one written (see
    /// [`Self::record_audit`]).
    audit_lost: Arc<std::sync::atomic::AtomicU64>,
    /// Credential and network pairs already noted (see `observe_address`).
    known_addresses: Arc<observability::KnownAddresses>,
    /// Applications a webhook said moved, waiting for the sync worker.
    hints: Arc<sync::Hints>,
    /// Public hostname serving webhook deliveries, `ingress.webhook_hostname`.
    webhook_hostname: Option<piqueld_core::manifest::Hostname>,
    /// Cancelled when the daemon shuts down, ending long-lived connections.
    shutdown: tokio_util::sync::CancellationToken,
    /// Branch listings shared by concurrent preview reads.
    listings: Arc<crate::git::Listings>,
    /// Budgets of expensive work (see [`Self::admit`]).
    admission: Arc<admission::Admission>,
    /// Sends notifications, queued or tests.
    notifier: notification_worker::Notifier,
    /// The website's origin, `auth.public_url`, which notifications link to.
    public_url: String,
}

/// Effective host settings, as attached by
/// [`ApplicationService::with_configuration`].
struct Configuration {
    /// Read-only settings for the API.
    view: piqueld_core::api::HostConfiguration,
    /// What installation checks inspect.
    checks: checks::CheckSettings,
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
            images: None,
            telemetry: crate::telemetry::Telemetry::default(),
            audit_backlog: Arc::new(tokio::sync::Semaphore::new(AUDIT_BACKLOG)),
            anonymous_backlog: Arc::new(tokio::sync::Semaphore::new(AUDIT_BACKLOG / 2)),
            audit_lost: Arc::default(),
            known_addresses: Arc::default(),
            hints: Arc::default(),
            webhook_hostname: None,
            shutdown: tokio_util::sync::CancellationToken::new(),
            listings: Arc::default(),
            admission: Arc::default(),
            notifier: notification_worker::Notifier::default(),
            public_url: String::new(),
        }
    }

    /// Links notifications to the dashboard at `public_url`, the website's origin.
    #[must_use]
    pub fn with_public_url(mut self, public_url: &str) -> Self {
        public_url
            .trim_end_matches('/')
            .clone_into(&mut self.public_url);
        self
    }

    /// Ends long-lived connections, like event streams and commands, once
    /// `shutdown` is cancelled.
    #[must_use]
    pub fn with_shutdown(mut self, shutdown: tokio_util::sync::CancellationToken) -> Self {
        self.shutdown = shutdown;
        self
    }

    /// Cancelled when the daemon shuts down.
    #[must_use]
    pub fn shutdown(&self) -> &tokio_util::sync::CancellationToken {
        &self.shutdown
    }

    /// Notifies after every change to accounts, credentials, or grants.
    #[must_use]
    pub fn access_changes(&self) -> tokio::sync::watch::Receiver<()> {
        self.store.access_changes()
    }

    /// `actor`'s current grants, for connections that outlive a request.
    /// # Errors
    /// Returns a revoked credential or storage errors.
    pub async fn grants(&self, actor: Actor<'_>) -> Result<Grants, ApplicationError> {
        Ok(self.store.grants(actor).await?)
    }

    /// Names the public hostname webhook deliveries arrive on, for the
    /// payload URLs shown to users.
    #[must_use]
    pub fn with_webhook_hostname(
        mut self,
        hostname: Option<piqueld_core::manifest::Hostname>,
    ) -> Self {
        self.webhook_hostname = hostname;
        self
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

    /// Exports through `telemetry`, already started, and reports its state.
    #[must_use]
    pub fn with_telemetry(mut self, telemetry: crate::telemetry::Telemetry) -> Self {
        self.telemetry = telemetry;
        self
    }

    /// Telemetry export, which records API latency.
    #[must_use]
    pub fn telemetry(&self) -> &crate::telemetry::Telemetry {
        &self.telemetry
    }

    /// Reports image cleanup's latest status.
    #[must_use]
    pub fn with_image_status(
        mut self,
        status: tokio::sync::watch::Receiver<piqueld_core::api::ImageStatus>,
    ) -> Self {
        self.images = Some(status);
        self
    }

    /// Shares managed ingress state with transport-independent daemon operations.
    #[must_use]
    pub fn with_ingress(mut self, ingress: Arc<crate::ingress::Ingress>) -> Self {
        self.ingress = Some(ingress);
        self
    }

    /// Attaches the effective host configuration for read-only API
    /// inspection and installation checks.
    #[must_use]
    pub fn with_configuration(mut self, config: &crate::config::DaemonConfig) -> Self {
        self.configuration = Some(Arc::new(Configuration {
            view: config.view(),
            checks: checks::CheckSettings::from(config),
        }));
        self
    }

    /// Recovers from a lost master key by discarding every stored and generated
    /// secret value and every webhook secret; see `Store::recover_secret_key`.
    /// # Errors
    /// Returns an error if the current key still works or is not lost, or
    /// storage errors.
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

    /// Removes an unreferenced generated secret and all of its runtime versions,
    /// holding off the controller's Docker mutations throughout (see
    /// `RuntimeBoundary::lock_mutations`). See `remove_secret_versions`.
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
        let _mutations = self.runtime.lock_mutations().await;
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
    /// environment's Docker secrets for it, holding off the controller's
    /// Docker mutations as `delete_secret` does. See `remove_secret_versions`.
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
        let _mutations = self.runtime.lock_mutations().await;
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
        mutation.check_submission(expected_generation, force, request_id)?;
        let receivers = mutation.changes_receivers();
        let (response, wake) = self
            .store
            .accept(actor, mutation, expected_generation, force, request_id)
            .await?;
        if wake {
            self.runtime.trigger_reconciliation();
        }
        if receivers {
            // Stops export to a revoked or deleted receiver before
            // answering, then lets the controller mount or unmount sockets.
            self.receive_telemetry().await?;
            self.runtime.trigger_full_scan();
        }
        Ok(response)
    }
}

#[cfg(test)]
impl ApplicationService {
    /// A service over `store` whose runtime never reaches Docker: its socket
    /// in `dir` does not exist, so any Docker call fails at once.
    pub(crate) fn unconnected(store: Arc<Store>, dir: &std::path::Path) -> Self {
        let docker =
            Arc::new(crate::docker::BollardDocker::connect(&dir.join("docker.sock")).unwrap());
        let runtime = crate::reconcile::Controller::new(docker, Arc::clone(&store))
            .runtime(Arc::new(tokio::sync::Notify::new()));
        Self::new(store, runtime)
    }
}
