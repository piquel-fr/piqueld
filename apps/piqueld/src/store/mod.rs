//! Integrated `SQLx` `SQLite` persistence implementation.
//!
//! `SQLx` owns the production `SQLite` pool, transactions, migrations, and
//! compile-time checked repository queries.

mod application;
mod operation;
mod status;

use crate::operations::OperationError;
use piqueld_core::{
    ApplicationId, NormalizedApplication, ResolutionSet, compile_application,
    resource::ResolvedApplication,
};
use serde::{Deserialize, Serialize};
use sqlx::{
    Sqlite, SqliteConnection, SqlitePool, Transaction,
    pool::PoolConnection,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{
    error::Error as StdError,
    fs,
    path::{Component, Path, PathBuf},
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
    /// The configured database path could not be prepared safely.
    #[error("database path could not be prepared")]
    PathSource(#[source] std::io::Error),
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

    fn path(source: std::io::Error) -> Self {
        Self::PathSource(source)
    }
}

/// Current persisted application status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationState {
    /// Application has been accepted but not started.
    Pending,
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
                    Self::Deploying | Self::Deleting | Self::Failed
                ) | (
                    Self::Deploying,
                    Self::Ready | Self::Degraded | Self::Failed | Self::Deleting
                ) | (Self::Ready, Self::Pending | Self::Degraded | Self::Deleting)
                    | (Self::Degraded, Self::Pending | Self::Ready | Self::Deleting)
                    | (Self::Failed, Self::Pending | Self::Deleting)
                    | (Self::Deleting, Self::Degraded | Self::Failed)
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
        }
    }
    fn parse(v: &str) -> Result<Self, StoreError> {
        match v {
            "create" => Ok(Self::Create),
            "replace" => Ok(Self::Replace),
            "delete" => Ok(Self::Delete),
            "reconcile" => Ok(Self::Reconcile),
            _ => Err(StoreError::Corrupt),
        }
    }
}

/// Durable operation lifecycle state.
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
    /// Whether an operation transition is valid.
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredApplication {
    /// Validated application manifest.
    pub application: NormalizedApplication,
    /// Immutable runtime resolution persisted with the desired generation.
    pub resolved: ResolvedApplication,
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

/// A bounded page of internal application rows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplicationPage {
    /// Rows in stable identifier order.
    pub items: Vec<StoredApplication>,
    /// Cursor for the next page, when more rows remain.
    pub next_cursor: Option<String>,
}
/// Result of an atomic desired-state mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutationResult {
    /// Generation created or requested by the mutation.
    pub generation: u64,
    /// Durable operation identifier.
    pub operation_id: String,
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
        let path = path.as_ref();
        prepare_database_path(path)?;
        let options = SqliteConnectOptions::new()
            .filename(path)
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
        // SQLx cannot construct queries for this statement with bindings.
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

/// Prepares only missing database parents and rejects symlinked path components.
/// Existing directories are never chmodded or otherwise modified. The final
/// database path is checked as well so a replaced symlink cannot be followed by
/// the `SQLite` driver during normal startup.
fn prepare_database_path(path: &Path) -> Result<(), StoreError> {
    let parent = path.parent().ok_or_else(|| {
        StoreError::invalid_input(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "database path has no parent",
        ))
    })?;

    let mut current = PathBuf::new();
    for component in parent.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(Path::new("/")),
            Component::CurDir => continue,
            Component::ParentDir => {
                return Err(StoreError::invalid_input(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "database path contains a parent component",
                )));
            }
            Component::Normal(name) => current.push(name),
        }

        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            }
            Ok(_) => {
                return Err(StoreError::path(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "database parent is not a real directory",
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(StoreError::path(error)),
                }
                let metadata = fs::symlink_metadata(&current).map_err(StoreError::path)?;
                if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                    return Err(StoreError::path(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "database parent was replaced by a non-directory",
                    )));
                }
            }
            Err(error) => return Err(StoreError::path(error)),
        }
    }

    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            Ok(())
        }
        Ok(_) => Err(StoreError::path(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "database path is not a regular file",
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StoreError::path(error)),
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
    let resolutions = ResolutionSet {
        sources: resolved
            .services
            .iter()
            .map(|service| (service.logical_name.clone(), service.source.clone()))
            .collect(),
    };
    let rebuilt = compile_application(app, resolved.instance_id.clone(), &resolutions)
        .map_err(|errors| StoreError::corrupt(CompilationErrors(errors)))?;
    (rebuilt == *resolved)
        .then_some(())
        .ok_or(StoreError::Corrupt)
}
fn canonical_resolved(
    app: &NormalizedApplication,
    value: &ResolvedApplication,
    instance_id: &str,
) -> Result<String, StoreError> {
    validate_resolved(app, value, instance_id)?;
    serde_json::to_string(value).map_err(StoreError::corrupt)
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
    resolved_json: String,
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
        let decoded = serde_json::from_str(&self.resolved_json).map_err(StoreError::corrupt)?;
        validate_resolved(&application, &decoded, instance_id)?;
        if serde_json::to_string(&decoded).map_err(StoreError::corrupt)? != self.resolved_json {
            return Err(StoreError::Corrupt);
        }
        Ok(StoredApplication {
            application,
            resolved: decoded,
            generation: u64::try_from(self.generation).map_err(StoreError::corrupt)?,
            spec_hash: self.spec_hash,
            delete_intent: self.delete_intent == 1,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
        })
    }
}
