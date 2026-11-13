//! The changes acceptance commits: validated application and environment
//! mutations, and the small responses stored to replay them.
use super::Promotion;
use piqueld_core::{
    ApplicationId, EnvironmentId, EnvironmentName, GitBranch, PreviewSlot, TrackedBranch,
    api::{LocalSnapshot, PromotionSelection},
    manifest::{ApplicationTemplate, RepositoryManifest, ValidatedTemplate},
};

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
        /// Build every Git source, even one whose image could be reused.
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        rebuild: bool,
    },
    /// Request an environment's resource deletion.
    Delete {
        /// Stable environment ID.
        id: EnvironmentId,
    },
    /// Start an environment and repair its accepted intent: its latest
    /// operation runs again once it has ended or failed.
    Reconcile {
        /// Stable environment ID.
        id: EnvironmentId,
    },
    /// Stop an environment: its services scale to zero and the gateway
    /// withdraws its routes until it is started or deployed again.
    Stop {
        /// Stable environment ID.
        id: EnvironmentId,
    },
    /// Deploy an earlier, succeeded deployment of an environment or preview
    /// again exactly as it ran. Needs no application revision: the
    /// deployment pins everything it deploys.
    Restore(Restore),
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

/// An earlier deployment to restore, through the endpoint of its kind: each
/// finds only environments, or only previews.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "of", rename_all = "snake_case")]
pub enum Restore {
    /// A deployment of an environment.
    Environment {
        /// Stable environment ID.
        id: EnvironmentId,
        /// The deployment to restore.
        deployment: String,
    },
    /// A deployment of a preview.
    Preview {
        /// Stable preview ID.
        id: EnvironmentId,
        /// The deployment to restore.
        deployment: String,
    },
}

impl Restore {
    /// The environment or preview, and the deployment of it to restore.
    #[must_use]
    pub fn parts(&self) -> (&EnvironmentId, &str) {
        match self {
            Self::Environment { id, deployment } | Self::Preview { id, deployment } => {
                (id, deployment)
            }
        }
    }
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
    /// [`Self::deploys_on_push`]). Previews need `previews:deploy` to create,
    /// deploy and restore, and `previews:delete` to delete, which `Deploy` and
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
            | Self::Stop { .. }
            | Self::Restore(Restore::Environment { .. })
            | Self::Sync(_) => &[Deploy],
            Self::Restore(Restore::Preview { .. })
            | Self::Preview(
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
    /// receive telemetry: granting or revoking it, or stopping or deleting
    /// environments. Started ones receive it again at the next refresh.
    #[must_use]
    pub const fn changes_receivers(&self) -> bool {
        matches!(
            self,
            Self::SetTelemetry { .. }
                | Self::Stop { .. }
                | Self::Delete { .. }
                | Self::DeleteApplication { .. }
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

    /// Whether submitting this change without `force` lacks the revision
    /// precondition it needs: every change but reconcile, stopping, restores, sync,
    /// telemetry grants and previews needs the inspected revision, and saves over a nonzero one also the
    /// inspected identity.
    #[must_use]
    pub const fn lacks_precondition(&self, expected_generation: Option<u64>) -> bool {
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
            | Self::Stop { .. }
            | Self::Restore(_)
            | Self::SetSync { .. }
            | Self::SetTelemetry { .. }
            | Self::Sync(_)
            | Self::Preview(_) => false,
        }
    }

    /// Deploys saved configuration at its configured manifest revision.
    #[must_use]
    pub fn deploy(id: EnvironmentId) -> Self {
        Self::Deploy {
            id,
            revision: None,
            rebuild: false,
        }
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
