//! Integrated `SQLx` `SQLite` persistence and repository contracts.
//!
//! `SQLx` owns the production `SQLite` pool, transactions, migrations, and
//! compile-time checked repository queries.

mod application;
mod build;
mod operation;
mod status;

use crate::operations::OperationError;
use async_trait::async_trait;
use piqueld_client::{
    ApplicationStatusView, ApplicationView, OperationStepView, OperationView, Page,
};
use piqueld_core::{
    ApplicationId, ErrorCode, NormalizedApplication, PublicError, ResolutionSet,
    compile_application,
    resource::{INGRESS_NETWORK, ResolvedApplication, SecretGeneration},
};
use serde::{Deserialize, Serialize};
use sqlx::{
    Sqlite, SqliteConnection, SqlitePool, Transaction,
    pool::PoolConnection,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{
    collections::BTreeMap,
    error::Error as StdError,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use uuid::Uuid;

macro_rules! include_migrations {
    () => {
        include!(concat!(env!("OUT_DIR"), "/migrations.rs"))
    };
}

const MIGRATIONS: &[&str] = include_migrations!();
/// Latest schema understood by this binary.
pub const SCHEMA_VERSION: u64 = MIGRATIONS.len() as u64;

#[derive(Debug, Error)]
#[error("application compilation failed: {0:?}")]
struct CompilationErrors(Vec<piqueld_core::CompileError>);

/// Persistence failures with stable classifications and retained source detail.
#[derive(Debug, Error)]
pub enum StoreError {
    /// A storage operation failed without a lower-level source.
    #[error("database operation failed")]
    Database,
    /// A storage operation failed in `SQLx`.
    #[error("database operation failed")]
    DatabaseSource(#[source] sqlx::Error),
    /// The database schema is newer or otherwise incompatible.
    #[error("database schema is incompatible")]
    SchemaMismatch,
    /// Schema metadata could not be read or decoded.
    #[error("database schema is incompatible")]
    SchemaMismatchSource(#[source] Box<dyn StdError + Send + Sync>),
    /// A requested row does not exist.
    #[error("resource was not found")]
    NotFound,
    /// A unique logical name or identifier already exists.
    #[error("resource already exists")]
    AlreadyExists,
    /// An idempotency key was previously bound to different normalized intent.
    #[error("idempotency key was reused for a different request")]
    IdempotencyConflict,
    /// An optimistic write used a stale generation.
    #[error("application generation conflict")]
    GenerationConflict {
        /// Generation supplied by the caller.
        expected: u64,
        /// Generation currently stored.
        actual: u64,
    },
    /// Persisted state failed domain revalidation or hash verification.
    #[error("stored application state is corrupt")]
    Corrupt,
    /// Persisted state could not be decoded into its domain representation.
    #[error("stored application state is corrupt")]
    CorruptSource(#[source] Box<dyn StdError + Send + Sync>),
    /// A manifest references logical secrets that do not exist.
    #[error("one or more referenced logical secrets do not exist")]
    MissingSecrets(Vec<String>),
    /// The requested durable state transition is illegal.
    #[error("illegal durable state transition")]
    IllegalTransition,
    /// A repository command contained malformed bounded metadata.
    #[error("repository input is invalid")]
    InvalidInput,
    /// Repository input could not be converted to its bounded representation.
    #[error("repository input is invalid")]
    InvalidInputSource(#[source] Box<dyn StdError + Send + Sync>),
}

impl PartialEq for StoreError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Database | Self::DatabaseSource(_),
                Self::Database | Self::DatabaseSource(_),
            )
            | (
                Self::SchemaMismatch | Self::SchemaMismatchSource(_),
                Self::SchemaMismatch | Self::SchemaMismatchSource(_),
            )
            | (Self::NotFound, Self::NotFound)
            | (Self::AlreadyExists, Self::AlreadyExists)
            | (Self::IdempotencyConflict, Self::IdempotencyConflict)
            | (Self::Corrupt | Self::CorruptSource(_), Self::Corrupt | Self::CorruptSource(_))
            | (Self::IllegalTransition, Self::IllegalTransition)
            | (
                Self::InvalidInput | Self::InvalidInputSource(_),
                Self::InvalidInput | Self::InvalidInputSource(_),
            ) => true,
            (
                Self::GenerationConflict {
                    expected: left_expected,
                    actual: left_actual,
                },
                Self::GenerationConflict {
                    expected: right_expected,
                    actual: right_actual,
                },
            ) => left_expected == right_expected && left_actual == right_actual,
            (Self::MissingSecrets(left), Self::MissingSecrets(right)) => left == right,
            _ => false,
        }
    }
}

impl Eq for StoreError {}

impl StoreError {
    fn database(source: sqlx::Error) -> Self {
        Self::DatabaseSource(source)
    }

    pub(crate) fn corrupt(source: impl StdError + Send + Sync + 'static) -> Self {
        Self::CorruptSource(Box::new(source))
    }

    fn schema_mismatch(source: impl StdError + Send + Sync + 'static) -> Self {
        Self::SchemaMismatchSource(Box::new(source))
    }

    fn invalid_input(source: impl StdError + Send + Sync + 'static) -> Self {
        Self::InvalidInputSource(Box::new(source))
    }

    /// Converts to the stable transport-neutral public contract without engine details.
    #[must_use]
    pub fn public(&self) -> PublicError {
        let (code, message) = match self {
            Self::NotFound => ("not_found", "resource was not found"),
            Self::AlreadyExists => ("already_exists", "resource already exists"),
            Self::IdempotencyConflict => (
                "idempotency_key_reused",
                "idempotency key was reused for a different request",
            ),
            Self::GenerationConflict { .. } => {
                ("generation_conflict", "application generation is stale")
            }
            Self::SchemaMismatch | Self::SchemaMismatchSource(_) => {
                ("schema_mismatch", "database schema is incompatible")
            }
            Self::Corrupt | Self::CorruptSource(_) => (
                "stored_state_corrupt",
                "stored application state is corrupt",
            ),
            Self::MissingSecrets(_) => (
                "logical_secret_missing",
                "one or more referenced logical secrets do not exist",
            ),
            Self::IllegalTransition => (
                "illegal_state_transition",
                "requested state transition is not allowed",
            ),
            Self::InvalidInput | Self::InvalidInputSource(_) => {
                ("invalid_argument", "repository input is invalid")
            }
            Self::Database | Self::DatabaseSource(_) => {
                ("storage_unavailable", "database operation failed")
            }
        };
        PublicError::new(ErrorCode::new(code), message)
    }
}

/// Current persisted application status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationState {
    /// Application has been accepted but not started.
    Pending,
    /// Mutable inputs are being resolved.
    Resolving,
    /// An image is being built.
    Building,
    /// Runtime resources are being reconciled.
    Deploying,
    /// Runtime state matches the desired generation.
    Ready,
    /// Runtime state is usable but not fully healthy.
    Degraded,
    /// Application deletion is in progress.
    Deleting,
    /// The latest operation failed.
    Failed,
}
impl ApplicationState {
    /// Returns the stable serialized state name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Resolving => "resolving",
            Self::Building => "building",
            Self::Deploying => "deploying",
            Self::Ready => "ready",
            Self::Degraded => "degraded",
            Self::Deleting => "deleting",
            Self::Failed => "failed",
        }
    }
    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "pending" => Ok(Self::Pending),
            "resolving" => Ok(Self::Resolving),
            "building" => Ok(Self::Building),
            "deploying" => Ok(Self::Deploying),
            "ready" => Ok(Self::Ready),
            "degraded" => Ok(Self::Degraded),
            "deleting" => Ok(Self::Deleting),
            "failed" => Ok(Self::Failed),
            _ => Err(StoreError::Corrupt),
        }
    }
    /// Whether a state change is valid for the durable application lifecycle.
    #[must_use]
    pub fn can_transition_to(self, next: Self) -> bool {
        self == next
            || matches!(
                (self, next),
                (
                    Self::Pending,
                    Self::Resolving
                        | Self::Building
                        | Self::Deploying
                        | Self::Deleting
                        | Self::Failed
                ) | (
                    Self::Resolving,
                    Self::Building
                        | Self::Deploying
                        | Self::Degraded
                        | Self::Failed
                        | Self::Deleting
                ) | (
                    Self::Building,
                    Self::Deploying | Self::Degraded | Self::Failed | Self::Deleting
                ) | (
                    Self::Deploying,
                    Self::Ready | Self::Degraded | Self::Failed | Self::Deleting
                ) | (
                    Self::Ready,
                    Self::Pending | Self::Resolving | Self::Degraded | Self::Deleting
                ) | (
                    Self::Degraded,
                    Self::Pending | Self::Resolving | Self::Ready | Self::Deleting
                ) | (
                    Self::Failed,
                    Self::Pending | Self::Resolving | Self::Deleting
                ) | (Self::Deleting, Self::Degraded | Self::Failed)
            )
    }
}

/// Durable operation category.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    /// Application creation operation.
    Create,
    /// Application replacement operation.
    Replace,
    /// Application deletion operation.
    Delete,
    /// Explicit reconciliation operation.
    Reconcile,
    /// Image build operation.
    Build,
    /// Runtime deployment operation.
    Deploy,
}
impl OperationKind {
    /// Returns the stable serialized operation name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Replace => "replace",
            Self::Delete => "delete",
            Self::Reconcile => "reconcile",
            Self::Build => "build",
            Self::Deploy => "deploy",
        }
    }
    fn parse(v: &str) -> Result<Self, StoreError> {
        match v {
            "create" => Ok(Self::Create),
            "replace" => Ok(Self::Replace),
            "delete" => Ok(Self::Delete),
            "reconcile" => Ok(Self::Reconcile),
            "build" => Ok(Self::Build),
            "deploy" => Ok(Self::Deploy),
            _ => Err(StoreError::Corrupt),
        }
    }
}

/// Durable operation/build lifecycle state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkState {
    /// Work has not started.
    Pending,
    /// Work is currently executing.
    Running,
    /// Work is recovering after interruption.
    Recovery,
    /// Work completed successfully.
    Succeeded,
    /// Work completed with an error.
    Failed,
    /// Work was cancelled.
    Cancelled,
}
impl WorkState {
    /// Returns the stable serialized work state.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Recovery => "recovery",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
    fn parse(v: &str) -> Result<Self, StoreError> {
        match v {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "recovery" => Ok(Self::Recovery),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(StoreError::Corrupt),
        }
    }
    /// Whether an operation/build transition is valid.
    #[must_use]
    pub fn can_transition_to(self, next: Self) -> bool {
        self == next
            || matches!(
                (self, next),
                (
                    Self::Pending | Self::Recovery,
                    Self::Running | Self::Cancelled
                ) | (
                    Self::Running,
                    Self::Recovery | Self::Succeeded | Self::Failed | Self::Cancelled
                )
            )
    }
    fn terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// Durable step lifecycle state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    /// Step has not started.
    Pending,
    /// Step is currently executing.
    Running,
    /// Step is recovering after interruption.
    Recovery,
    /// Step completed successfully.
    Succeeded,
    /// Step completed with an error.
    Failed,
    /// Step was cancelled.
    Cancelled,
    /// Step was no longer needed.
    Skipped,
}
impl StepState {
    /// Returns the stable serialized step state.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Recovery => "recovery",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
        }
    }
    fn parse(state: &str) -> Result<Self, StoreError> {
        Ok(match state {
            "pending" => Self::Pending,
            "running" => Self::Running,
            "recovery" => Self::Recovery,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "skipped" => Self::Skipped,
            _ => return Err(StoreError::Corrupt),
        })
    }

    /// Whether a step transition is valid.
    #[must_use]
    pub fn can_transition_to(self, next: Self) -> bool {
        self == next
            || matches!(
                (self, next),
                (
                    Self::Pending | Self::Recovery,
                    Self::Running | Self::Cancelled | Self::Skipped
                ) | (
                    Self::Running,
                    Self::Recovery | Self::Succeeded | Self::Failed | Self::Cancelled
                )
            )
    }
    fn terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Skipped
        )
    }
}

/// Revalidated current application state.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredApplication {
    /// Validated application manifest.
    pub application: NormalizedApplication,
    /// Immutable runtime resolution, when available.
    pub resolved: Option<ResolvedApplication>,
    /// Monotonic application generation.
    pub generation: u64,
    /// Hash of the normalized desired specification.
    pub spec_hash: String,
    /// Whether deletion has been requested.
    pub delete_intent: bool,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: i64,
    /// Last update timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
}

/// Default number of rows returned by a bounded repository query.
pub const DEFAULT_PAGE_SIZE: usize = 50;
/// Maximum number of rows processed by one bounded repository query.
pub const MAX_PAGE_SIZE: usize = 100;

impl StoredApplication {
    /// Converts the stored row to the public API view.
    #[must_use]
    pub fn view(self) -> ApplicationView {
        ApplicationView {
            application: self.application,
            generation: self.generation,
            spec_hash: self.spec_hash,
            delete_intent: self.delete_intent,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
        }
    }
}

/// Application status row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplicationStatus {
    /// Stable application identifier.
    pub application_id: ApplicationId,
    /// Current durable lifecycle state.
    pub state: ApplicationState,
    /// Last runtime generation observed as converged.
    pub observed_generation: Option<u64>,
    /// Optional safe status message.
    pub message: Option<String>,
    /// Last update timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
}
impl ApplicationStatus {
    /// Converts the stored row to the public API view.
    #[must_use]
    pub fn view(self) -> ApplicationStatusView {
        ApplicationStatusView {
            application_id: self.application_id.to_string(),
            state: self.state.as_str().into(),
            observed_generation: self.observed_generation,
            message: self.message,
            updated_at_ms: self.updated_at_ms,
        }
    }
}

/// Durable operation row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Operation {
    /// Stable operation identifier.
    pub id: String,
    /// Stable application identifier.
    pub application_id: ApplicationId,
    /// Application generation associated with the operation.
    pub generation: u64,
    /// Durable operation category.
    pub kind: OperationKind,
    /// Current operation state.
    pub state: WorkState,
    /// Stable failure code, when present.
    pub error_code: Option<String>,
    /// Safe failure message, when present.
    pub error_message: Option<String>,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: i64,
    /// Last update timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
    /// Start timestamp in Unix milliseconds.
    pub started_at_ms: Option<i64>,
    /// Completion timestamp in Unix milliseconds.
    pub finished_at_ms: Option<i64>,
}
impl Operation {
    /// Converts the stored row and steps to the public API view.
    #[must_use]
    pub fn view(self, steps: Vec<OperationStep>) -> OperationView {
        OperationView {
            id: self.id,
            application_id: self.application_id.to_string(),
            generation: self.generation,
            kind: self.kind.as_str().into(),
            state: self.state.as_str().into(),
            error_code: self.error_code,
            error_message: self.error_message,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            started_at_ms: self.started_at_ms,
            finished_at_ms: self.finished_at_ms,
            steps: steps
                .into_iter()
                .map(|s| OperationStepView {
                    id: s.id,
                    position: s.position,
                    action: s.action,
                    state: s.state.as_str().into(),
                    attempt: s.attempt,
                    error_code: s.error_code,
                    error_message: s.error_message,
                    updated_at_ms: s.updated_at_ms,
                })
                .collect(),
        }
    }
}

/// Durable operation step row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationStep {
    /// Stable step identifier.
    pub id: String,
    /// Owning operation identifier.
    pub operation_id: String,
    /// Position in the operation plan.
    pub position: u32,
    /// Stable action identifier.
    pub action: String,
    /// Current step state.
    pub state: StepState,
    /// Number of execution attempts.
    pub attempt: u32,
    /// Stable failure code, when present.
    pub error_code: Option<String>,
    /// Safe failure message, when present.
    pub error_message: Option<String>,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: i64,
    /// Last update timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
    /// Start timestamp in Unix milliseconds.
    pub started_at_ms: Option<i64>,
    /// Completion timestamp in Unix milliseconds.
    pub finished_at_ms: Option<i64>,
}

/// Durable build row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Build {
    /// Stable build identifier.
    pub id: String,
    /// Owning operation identifier.
    pub operation_id: String,
    /// Stable application identifier.
    pub application_id: ApplicationId,
    /// Logical service being built.
    pub service_name: String,
    /// Current build state.
    pub state: WorkState,
    /// Resolved source commit, when available.
    pub source_commit: Option<String>,
    /// Registry image reference, when available.
    pub image_reference: Option<String>,
    /// Immutable image digest, when available.
    pub image_digest: Option<String>,
    /// Stable failure code, when present.
    pub error_code: Option<String>,
    /// Safe failure message, when present.
    pub error_message: Option<String>,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: i64,
    /// Last update timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
    /// Start timestamp in Unix milliseconds.
    pub started_at_ms: Option<i64>,
    /// Completion timestamp in Unix milliseconds.
    pub finished_at_ms: Option<i64>,
}
/// Result of an atomic desired-state mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutationResult {
    /// Generation created or requested by the mutation.
    pub generation: u64,
    /// Durable operation identifier.
    pub operation_id: String,
}

/// Application persistence contract used by later API/reconciliation layers.
#[async_trait]
pub trait ApplicationRepository: Send + Sync {
    /// Atomically creates an application and its initial operation.
    async fn create(
        &self,
        app: &NormalizedApplication,
        resolved: Option<&ResolvedApplication>,
        steps: &[String],
    ) -> Result<MutationResult, StoreError>;
    /// Atomically creates an application or returns the original create result
    /// for a durable idempotency-key binding.
    async fn create_idempotent(
        &self,
        app: &NormalizedApplication,
        resolved: Option<&ResolvedApplication>,
        steps: &[String],
        key_hash: &str,
        request_hash: &str,
    ) -> Result<MutationResult, StoreError>;
    /// Looks up an existing create key without invoking runtime preparation.
    async fn create_idempotency(
        &self,
        app_id: &ApplicationId,
        key_hash: &str,
        request_hash: &str,
    ) -> Result<Option<MutationResult>, StoreError>;
    /// Replaces an application at an expected generation.
    async fn replace(
        &self,
        app: &NormalizedApplication,
        resolved: Option<&ResolvedApplication>,
        expected_generation: u64,
        steps: &[String],
    ) -> Result<MutationResult, StoreError>;
    /// Requests deletion at an expected generation.
    async fn request_delete(
        &self,
        id: &ApplicationId,
        expected_generation: u64,
        steps: &[String],
    ) -> Result<MutationResult, StoreError>;
    /// Hides a successfully removed application while retaining its operation journal.
    async fn finalize_delete(&self, id: &ApplicationId, generation: u64) -> Result<(), StoreError>;
    /// Atomically creates, or returns, durable reconcile work for a generation.
    async fn request_reconcile(
        &self,
        id: &ApplicationId,
        expected_generation: u64,
        steps: &[String],
    ) -> Result<MutationResult, StoreError>;
    /// Returns already-durable active reconcile work for a generation.
    async fn active_reconcile(
        &self,
        id: &ApplicationId,
        expected_generation: u64,
    ) -> Result<Option<MutationResult>, StoreError>;
    /// Fetches an application by identifier.
    async fn get(&self, id: &ApplicationId) -> Result<StoredApplication, StoreError>;
    /// Looks up an active application by user-facing name.
    async fn find_by_name(&self, name: &str) -> Result<Option<StoredApplication>, StoreError>;
    /// Lists active applications using a bounded cursor query.
    async fn list(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<StoredApplication>, StoreError>;
}
/// Operation journal persistence contract.
#[async_trait]
pub trait OperationRepository: Send + Sync {
    /// Fetches one durable operation.
    async fn operation(&self, operation_id: &str) -> Result<Operation, StoreError>;
    /// Fetches the ordered steps for an operation.
    async fn operation_steps(&self, operation_id: &str) -> Result<Vec<OperationStep>, StoreError>;
    /// Lists recent operations for an application.
    async fn operations_for_application(
        &self,
        application_id: &ApplicationId,
        limit: usize,
    ) -> Result<Vec<Operation>, StoreError>;
    /// Lists pending or recoverable operations.
    async fn pending_operations(&self, limit: usize) -> Result<Vec<Operation>, StoreError>;
    /// Applies a checked state transition to an operation.
    async fn transition_operation(
        &self,
        operation_id: &str,
        from: WorkState,
        to: WorkState,
        error: Option<OperationError>,
    ) -> Result<(), StoreError>;
    /// Applies a checked state transition to an operation step.
    async fn transition_step(
        &self,
        step_id: &str,
        from: StepState,
        to: StepState,
        error: Option<OperationError>,
    ) -> Result<(), StoreError>;
    /// Moves interrupted running work into recovery.
    async fn recover_interrupted(&self) -> Result<u64, StoreError>;
    /// Removes a bounded batch of old finished operations.
    async fn prune_finished_operations_before(
        &self,
        cutoff_ms: i64,
        limit: usize,
    ) -> Result<u64, StoreError>;
}
/// Status persistence contract.
#[async_trait]
pub trait StatusRepository: Send + Sync {
    /// Fetches the current status for an application.
    async fn status(&self, id: &ApplicationId) -> Result<ApplicationStatus, StoreError>;
    /// Applies a checked status transition.
    async fn set_status(
        &self,
        id: &ApplicationId,
        from: ApplicationState,
        to: ApplicationState,
        observed_generation: Option<u64>,
        message: Option<&str>,
    ) -> Result<(), StoreError>;
}
/// Build persistence and retention contract.
#[async_trait]
pub trait BuildRepository: Send + Sync {
    /// Creates a build row for an operation.
    async fn create_build(
        &self,
        operation_id: &str,
        application_id: &ApplicationId,
        service_name: &str,
    ) -> Result<Build, StoreError>;
    /// Fetches one build row.
    async fn build(&self, id: &str) -> Result<Build, StoreError>;
    /// Lists builds belonging to an operation.
    async fn builds_for_operation(&self, operation_id: &str) -> Result<Vec<Build>, StoreError>;
    /// Records immutable output produced by a build.
    async fn record_build_output(
        &self,
        id: &str,
        source_commit: &str,
        image_reference: &str,
        image_digest: &str,
    ) -> Result<(), StoreError>;
    /// Applies a checked state transition to a build.
    async fn transition_build(
        &self,
        id: &str,
        from: WorkState,
        to: WorkState,
        error: Option<OperationError>,
    ) -> Result<(), StoreError>;
    /// Removes a bounded batch of old finished builds.
    async fn prune_finished_before(&self, cutoff_ms: i64, limit: usize) -> Result<u64, StoreError>;
}

/// Integrated `SQLx` `SQLite` repository implementation.
#[derive(Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
    instance_id: String,
}

impl SqliteStore {
    /// Opens a local `SQLite` database, applies forward migrations, and creates or loads its instance ID.
    ///
    /// # Errors
    /// Returns a sanitized storage or schema compatibility error.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let options = SqliteConnectOptions::new()
            .filename(path.as_ref())
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await
            .map_err(StoreError::database)?;

        // SQLite does not expose PRAGMA assignment through bind parameters, so
        // the schema-version assignment below is the only dynamically assembled
        // statement in the store.
        let version: i64 = sqlx::query_scalar!("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .map_err(StoreError::database)?
            .ok_or(StoreError::Database)?;
        let version = u64::try_from(version).map_err(StoreError::schema_mismatch)?;
        if version > SCHEMA_VERSION {
            return Err(StoreError::SchemaMismatch);
        }
        if version > 0 {
            let recorded = sqlx::query_scalar!(
                "SELECT schema_version FROM instance_metadata WHERE singleton=1"
            )
            .fetch_optional(&pool)
            .await
            .map_err(StoreError::schema_mismatch)?
            .ok_or(StoreError::SchemaMismatch)?;
            if u64::try_from(recorded).map_err(StoreError::schema_mismatch)? != version {
                return Err(StoreError::SchemaMismatch);
            }
        }

        let migration_start = usize::try_from(version).map_err(StoreError::schema_mismatch)?;
        for (index, migration) in MIGRATIONS.iter().enumerate().skip(migration_start) {
            let mut tx = pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(StoreError::database)?;
            sqlx::raw_sql(migration)
                .execute(&mut *tx)
                .await
                .map_err(StoreError::database)?;
            Self::set_user_version(&mut tx, index + 1).await?;
            tx.commit().await.map_err(StoreError::database)?;
        }

        let now = now_ms();
        let generated = format!("instance-{}", Uuid::now_v7().simple());
        let schema_version = i64::try_from(SCHEMA_VERSION).map_err(StoreError::schema_mismatch)?;
        sqlx::query!(
            "INSERT OR IGNORE INTO instance_metadata(singleton,instance_id,schema_version,created_at_ms) VALUES(1,?1,?2,?3)",
            generated,
            schema_version,
            now
        )
        .execute(&pool)
        .await
        .map_err(StoreError::database)?;
        let row = sqlx::query!(
            "SELECT instance_id,schema_version FROM instance_metadata WHERE singleton=1"
        )
        .fetch_optional(&pool)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::Corrupt)?;
        let instance_id = row.instance_id;
        piqueld_core::InstanceId::parse(instance_id.clone()).map_err(StoreError::corrupt)?;
        let metadata_version = u64::try_from(row.schema_version).map_err(StoreError::corrupt)?;
        if metadata_version != SCHEMA_VERSION {
            return Err(StoreError::SchemaMismatch);
        }
        Ok(Self { pool, instance_id })
    }

    async fn set_user_version(
        tx: &mut Transaction<'_, Sqlite>,
        version: usize,
    ) -> Result<(), StoreError> {
        let statement = format!("PRAGMA user_version = {version}");
        // sqlc cant construct queries for this statement with bindings.
        sqlx::query(&statement)
            .execute(&mut **tx)
            .await
            .map(|_| ())
            .map_err(StoreError::database)
    }

    /// Stable identity of this control-plane database.
    #[must_use]
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    async fn connection(&self) -> Result<PoolConnection<Sqlite>, StoreError> {
        self.pool.acquire().await.map_err(StoreError::database)
    }

    async fn begin_immediate(&self) -> Result<Transaction<'static, Sqlite>, StoreError> {
        self.pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(StoreError::database)
    }

    /// Inserts logical-secret metadata for existence validation. Encryption lifecycle arrives in Plan 07.
    ///
    /// # Errors
    /// Returns `AlreadyExists` for duplicate names or a sanitized storage error.
    pub async fn declare_logical_secret(&self, name: &str) -> Result<(), StoreError> {
        if !valid_logical_name(name) {
            return Err(StoreError::InvalidInput);
        }
        let now = now_ms();
        let id = new_id("secret");
        let inserted = sqlx::query!(
            "INSERT OR IGNORE INTO logical_secrets(id,name,generation,value_is_set,created_at_ms,updated_at_ms) VALUES(?1,?2,1,0,?3,?3)",
            id,
            name,
            now
        )
        .execute(&self.pool)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        if inserted != 1 {
            return Err(StoreError::AlreadyExists);
        }
        Ok(())
    }

    async fn validate_secrets(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        app: &NormalizedApplication,
    ) -> Result<(), StoreError> {
        let names: Vec<String> = app
            .spec
            .services
            .iter()
            .flat_map(|service| service.secrets.iter().map(|secret| secret.source.clone()))
            .collect();
        let mut missing = Vec::new();
        for name in names {
            let exists = sqlx::query_scalar!(
                r#"SELECT 1 AS "present!: i64" FROM logical_secrets WHERE name=?1"#,
                name
            )
            .fetch_optional(&mut **tx)
            .await
            .map_err(StoreError::database)?
            .is_some();
            if !exists {
                missing.push(name);
            }
        }
        if missing.is_empty() {
            Ok(())
        } else {
            missing.sort();
            missing.dedup();
            Err(StoreError::MissingSecrets(missing))
        }
    }

    async fn transition_miss(
        connection: &mut SqliteConnection,
        table: &'static str,
        id: &str,
    ) -> Result<StoreError, StoreError> {
        let exists = match table {
            "operations" => {
                sqlx::query_scalar!(
                    r#"SELECT 1 AS "present!: i64" FROM operations WHERE id=?1"#,
                    id
                )
                .fetch_optional(&mut *connection)
                .await
            }
            "operation_steps" => {
                sqlx::query_scalar!(
                    r#"SELECT 1 AS "present!: i64" FROM operation_steps WHERE id=?1"#,
                    id
                )
                .fetch_optional(&mut *connection)
                .await
            }
            "application_status" => sqlx::query_scalar!(
                r#"SELECT 1 AS "present!: i64" FROM application_status WHERE application_id=?1"#,
                id
            )
            .fetch_optional(&mut *connection)
            .await,
            "builds" => {
                sqlx::query_scalar!(r#"SELECT 1 AS "present!: i64" FROM builds WHERE id=?1"#, id)
                    .fetch_optional(&mut *connection)
                    .await
            }
            _ => return Err(StoreError::Database),
        }
        .map_err(StoreError::database)?
        .is_some();
        Ok(if exists {
            StoreError::IllegalTransition
        } else {
            StoreError::NotFound
        })
    }

    async fn insert_operation(
        tx: &mut Transaction<'_, Sqlite>,
        app_id: &ApplicationId,
        generation: u64,
        kind: OperationKind,
        steps: &[String],
        now: i64,
    ) -> Result<String, StoreError> {
        validate_operation_steps(steps)?;
        let id = new_id("operation");
        let app_id = app_id.as_str();
        let generation = generation_i64(generation)?;
        let operation_kind = kind.as_str();
        sqlx::query!(
            "INSERT INTO operations(id,application_id,generation,kind,state,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,'pending',?5,?5)",
            id,
            app_id,
            generation,
            operation_kind,
            now
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Self::insert_operation_steps(tx, &id, steps, now).await?;
        Ok(id)
    }

    async fn insert_operation_steps(
        tx: &mut Transaction<'_, Sqlite>,
        operation_id: &str,
        steps: &[String],
        now: i64,
    ) -> Result<(), StoreError> {
        for (position, action) in steps.iter().enumerate() {
            let step_id = new_id("step");
            let position = i64::try_from(position).map_err(StoreError::invalid_input)?;
            sqlx::query!(
                "INSERT INTO operation_steps(id,operation_id,position,action,state,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,'pending',?5,?5)",
                step_id,
                operation_id,
                position,
                action,
                now
            )
            .execute(&mut **tx)
            .await
            .map_err(StoreError::database)?;
        }
        Ok(())
    }
}

fn now_ms() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

fn generation_i64(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(StoreError::corrupt)
}
fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::now_v7().simple())
}
fn valid_logical_name(value: &str) -> bool {
    (1..=63).contains(&value.len())
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !value.ends_with('-')
}
fn valid_bounded_text(value: &str, max_len: usize) -> bool {
    !value.is_empty() && value.len() <= max_len && !value.chars().any(char::is_control)
}
fn validate_operation_steps(steps: &[String]) -> Result<(), StoreError> {
    if steps
        .iter()
        .any(|step| step.is_empty() || step.len() > 64 || step.chars().any(char::is_control))
    {
        Err(StoreError::InvalidInput)
    } else {
        Ok(())
    }
}
fn page_limit(limit: usize) -> Result<i64, StoreError> {
    if !(1..=MAX_PAGE_SIZE).contains(&limit) {
        return Err(StoreError::InvalidInput);
    }
    i64::try_from(limit).map_err(StoreError::invalid_input)
}
fn valid_sha256(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
}
fn validate_resolved(
    app: &NormalizedApplication,
    resolved: &ResolvedApplication,
    instance_id: &str,
) -> Result<(), StoreError> {
    if resolved.id != app.id
        || resolved.name != app.metadata.name.as_str()
        || resolved.spec_hash != app.spec_hash()
        || resolved.instance_id.as_str() != instance_id
    {
        return Err(StoreError::Corrupt);
    }
    let ingress = resolved
        .networks
        .iter()
        .filter(|network| network.ingress)
        .map(|network| network.name.as_str())
        .collect::<Vec<_>>();
    let has_routes = !app.spec.routes.is_empty();
    if ingress.len() != usize::from(has_routes) {
        return Err(StoreError::Corrupt);
    }
    let resolutions = ResolutionSet {
        sources: resolved
            .services
            .iter()
            .map(|service| (service.logical_name.clone(), service.source.clone()))
            .collect(),
        secrets: resolved
            .secrets
            .iter()
            .map(|secret| {
                (
                    secret.logical_name.clone(),
                    SecretGeneration {
                        logical_name: secret.logical_name.clone(),
                        generation: secret.generation.clone(),
                        swarm_name: secret.name.clone(),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>(),
    };
    let ingress_name = ingress.first().copied().unwrap_or(INGRESS_NETWORK);
    let rebuilt = compile_application(
        app,
        resolved.instance_id.clone(),
        ingress_name,
        &resolutions,
    )
    .map_err(|errors| StoreError::corrupt(CompilationErrors(errors)))?;
    (rebuilt == *resolved)
        .then_some(())
        .ok_or(StoreError::Corrupt)
}
fn canonical_resolved(
    app: &NormalizedApplication,
    value: Option<&ResolvedApplication>,
    instance_id: &str,
) -> Result<Option<String>, StoreError> {
    value
        .map(|resolved| {
            validate_resolved(app, resolved, instance_id)?;
            serde_json::to_string(resolved).map_err(StoreError::corrupt)
        })
        .transpose()
}
fn decode_application(
    json: &str,
    expected_hash: &str,
) -> Result<NormalizedApplication, StoreError> {
    let raw: NormalizedApplication = serde_json::from_str(json).map_err(StoreError::corrupt)?;
    // Round-trip through the strict public parser to restore all semantic invariants.
    let toml = raw.export_toml().map_err(StoreError::corrupt)?;
    let validated = piqueld_core::parse_toml(&toml).map_err(StoreError::corrupt)?;
    let app = validated.normalize(raw.id.clone());
    if app.spec_hash() != expected_hash
        || app.canonical_json().map_err(StoreError::corrupt)? != json
    {
        return Err(StoreError::Corrupt);
    }
    Ok(app)
}

#[derive(Debug)]
struct ApplicationRow {
    id: String,
    name: String,
    desired_json: String,
    resolved_json: Option<String>,
    generation: i64,
    spec_hash: String,
    delete_intent: i64,
    created_at_ms: i64,
    updated_at_ms: i64,
}

impl ApplicationRow {
    fn decode(self, instance_id: &str) -> Result<StoredApplication, StoreError> {
        let application = decode_application(&self.desired_json, &self.spec_hash)?;
        if application.id.as_str() != self.id || application.metadata.name.as_str() != self.name {
            return Err(StoreError::Corrupt);
        }
        let resolved: Option<ResolvedApplication> = self
            .resolved_json
            .map(|json| {
                let decoded = serde_json::from_str(&json).map_err(StoreError::corrupt)?;
                validate_resolved(&application, &decoded, instance_id)?;
                if serde_json::to_string(&decoded).map_err(StoreError::corrupt)? != json {
                    return Err(StoreError::Corrupt);
                }
                Ok(decoded)
            })
            .transpose()?;
        Ok(StoredApplication {
            application,
            resolved,
            generation: u64::try_from(self.generation).map_err(StoreError::corrupt)?,
            spec_hash: self.spec_hash,
            delete_intent: self.delete_intent == 1,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
        })
    }
}
