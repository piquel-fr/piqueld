//! SQLite persistence and atomic acceptance of validated application commands;
//! Docker planning and execution belong to the controller.

mod acceptance;
mod application;
mod event;
mod operation;
mod status;

use piqueld_core::{ApplicationId, NormalizedApplication, resource::ResolvedApplication};
pub use piqueld_core::{ApplicationState, Operation, OperationKind, OperationState};
use sqlx::{
    Sqlite, SqlitePool, Transaction,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{
    error::Error as StdError,
    fs,
    path::Path,
    sync::atomic::{AtomicI64, Ordering},
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
/// Latest database schema.
pub const SCHEMA_VERSION: u64 = MIGRATIONS.len() as u64;

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
    /// The requested intent revision is stale.
    #[error("application generation conflict: expected {expected}, actual {actual}")]
    GenerationConflict {
        /// Requested revision.
        expected: u64,
        /// Current revision; zero means absent.
        actual: u64,
    },
    /// The name no longer selects the application inspected by the caller.
    #[error("application identity changed since inspection")]
    IdentityConflict,
    /// A request ID was already used for different input.
    #[error("request ID was already used for different input")]
    ReplayConflict,
    /// Mutation cannot run during pending work or deletion.
    #[error("application is busy; wait for its current operation to finish")]
    Busy,
    /// A unique logical name or identifier already exists.
    #[error("resource already exists")]
    AlreadyExists,
    /// Persisted state has inconsistent identity.
    #[error("stored application state is corrupt")]
    Corrupt,
    /// Persisted state could not be decoded into its domain representation.
    #[error("stored application state is corrupt")]
    CorruptSource(#[source] Box<dyn StdError + Send + Sync>),
    /// The requested durable state transition is illegal.
    #[error("illegal durable state transition")]
    IllegalTransition,
    /// A repository command contained invalid input.
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

/// Persisted application target. Runtime status is recorded separately.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredApplication {
    /// Validated, normalized manifest.
    pub application: NormalizedApplication,
    /// Resolved Docker target, including immutable image digests.
    pub resolved: Option<ResolvedApplication>,
    /// Current intent revision.
    pub generation: u64,
    /// Revision associated with the last resolved target.
    pub resolved_generation: Option<u64>,
    /// Whether the target is absence of services and networks.
    pub delete_intent: bool,
    /// When this application was created.
    pub created_at_ms: i64,
    /// When its target last changed.
    pub updated_at_ms: i64,
}

/// Default query page size.
pub const DEFAULT_PAGE_SIZE: usize = 50;
/// Maximum query page size.
pub const MAX_PAGE_SIZE: usize = 100;

/// Last observed application status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplicationStatus {
    /// Application identity.
    pub application_id: ApplicationId,
    /// Current status.
    pub state: ApplicationState,
    /// Last observed runtime health, independent of pending intent.
    pub runtime_health: Option<String>,
    /// Latest diagnostic, when present.
    pub message: Option<String>,
    /// Observation timestamp.
    pub updated_at_ms: i64,
}

/// Page of live application records.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplicationPage {
    /// Applications in ID order.
    pub items: Vec<StoredApplication>,
    /// Cursor for the next page.
    pub next_cursor: Option<String>,
}

/// SQLite repository shared by the application service and controller.
#[derive(Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
    instance_id: String,
    writers: std::sync::Arc<tokio::sync::Mutex<()>>,
}

impl SqliteStore {
    /// Opens a local `SQLite` database, applies forward migrations, and creates or loads its instance ID.
    ///
    /// # Errors
    /// Returns a sanitized storage or schema compatibility error.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        ensure_database_target(path)?;
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(5))
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
            .map_err(StoreError::database)?
            .ok_or(StoreError::SchemaMismatch)?;
            if u64::try_from(recorded).map_err(StoreError::schema_mismatch)? != version {
                return Err(StoreError::SchemaMismatch);
            }
        }

        let migration_start = usize::try_from(version).map_err(StoreError::schema_mismatch)?;
        let now = now_ms();
        let generated = format!("instance-{}", Uuid::now_v7().simple());
        let schema_version = i64::try_from(SCHEMA_VERSION).map_err(StoreError::schema_mismatch)?;
        let final_version = MIGRATIONS.len();
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
            if index + 1 == final_version {
                // Commit the instance metadata row atomically with the last
                // version bump so a crash can never migrate without identity.
                sqlx::query!(
                    "INSERT INTO instance_metadata(singleton,instance_id,schema_version,created_at_ms) VALUES(1,?1,?2,?3) ON CONFLICT(singleton) DO UPDATE SET schema_version=excluded.schema_version",
                    generated,
                    schema_version,
                    now
                )
                .execute(&mut *tx)
                .await
                .map_err(StoreError::database)?;
            }
            tx.commit().await.map_err(StoreError::database)?;
        }

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
        Ok(Self {
            pool,
            instance_id,
            writers: std::sync::Arc::default(),
        })
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

    // Queue writers asynchronously before acquiring SQLite's single write lock.
    // Competing BEGIN requests otherwise occupy pool workers and can starve a
    // transaction under concurrent reconciliation.
    pub(crate) async fn begin_immediate(
        &self,
    ) -> Result<
        (
            tokio::sync::MutexGuard<'_, ()>,
            Transaction<'static, Sqlite>,
        ),
        StoreError,
    > {
        let writer = self.writers.lock().await;
        let tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(StoreError::database)?;
        Ok((writer, tx))
    }
}

/// Verifies the database target is a regular file or absent before `SQLite`
/// creates it. The parent directory's privacy is enforced by the daemon's data
/// directory preparation, not here.
fn ensure_database_target(path: &Path) -> Result<(), StoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.is_symlink() => Ok(()),
        Ok(_) => Err(StoreError::path(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "database path is not a regular file",
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StoreError::path(error)),
    }
}

static LAST_NOW_MS: AtomicI64 = AtomicI64::new(0);

/// Returns the current Unix time in milliseconds, monotonic within this
/// process so clock step-backs can never violate schema timestamp checks.
fn now_ms() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let observed = i64::try_from(millis).unwrap_or(i64::MAX);
    let mut last = LAST_NOW_MS.load(Ordering::Relaxed);
    loop {
        let next = last.saturating_add(1).max(observed);
        match LAST_NOW_MS.compare_exchange_weak(last, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(current) => last = current,
        }
    }
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::now_v7().simple())
}
fn page_limit(limit: usize) -> Result<i64, StoreError> {
    if !(1..=MAX_PAGE_SIZE).contains(&limit) {
        return Err(StoreError::InvalidInput);
    }
    i64::try_from(limit).map_err(StoreError::invalid_input)
}

#[derive(Debug)]
struct ApplicationRow {
    id: String,
    desired_json: String,
    resolved_json: Option<String>,
    generation: i64,
    resolved_generation: Option<i64>,
    delete_intent: i64,
    created_at_ms: i64,
    updated_at_ms: i64,
}
impl ApplicationRow {
    fn decode(self) -> Result<StoredApplication, StoreError> {
        let application: NormalizedApplication =
            serde_json::from_str(&self.desired_json).map_err(StoreError::corrupt)?;
        if application.id.as_str() != self.id {
            return Err(StoreError::Corrupt);
        }
        Ok(StoredApplication {
            application,
            resolved: self
                .resolved_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(StoreError::corrupt)?,
            generation: u64::try_from(self.generation).map_err(StoreError::corrupt)?,
            resolved_generation: self
                .resolved_generation
                .map(u64::try_from)
                .transpose()
                .map_err(StoreError::corrupt)?,
            delete_intent: self.delete_intent != 0,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
        })
    }
}
