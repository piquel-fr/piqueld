//! Embedded libSQL persistence and repository contracts.
//!
//! The official libSQL SDK is the only production database runtime. `SQLx` is used
//! by an isolated integration test to validate these migrations and query shapes.
#![allow(missing_docs)]
#![allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

use async_trait::async_trait;
use libsql::{Builder, Connection, Database, Transaction, params};
use piqueld_core::{ApplicationId, ErrorCode, NormalizedApplication, PublicError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use uuid::Uuid;

const MIGRATIONS: &[&str] = &[
    include_str!("../../../../migrations/0001_control_plane.sql"),
    include_str!("../../../../migrations/0002_retention_indexes.sql"),
];
/// Latest schema understood by this binary.
pub const SCHEMA_VERSION: u64 = MIGRATIONS.len() as u64;

/// Stable, sanitized persistence failures.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum StoreError {
    /// A storage operation failed. The underlying engine text is deliberately not retained.
    #[error("database operation failed")]
    Database,
    /// The database schema is newer or otherwise incompatible.
    #[error("database schema is incompatible")]
    SchemaMismatch,
    /// A requested row does not exist.
    #[error("resource was not found")]
    NotFound,
    /// A unique logical name or identifier already exists.
    #[error("resource already exists")]
    AlreadyExists,
    /// An optimistic write used a stale generation.
    #[error("application generation conflict")]
    GenerationConflict { expected: u64, actual: u64 },
    /// Persisted state failed domain revalidation or hash verification.
    #[error("stored application state is corrupt")]
    Corrupt,
    /// A manifest references logical secrets that do not exist.
    #[error("one or more referenced logical secrets do not exist")]
    MissingSecrets(Vec<String>),
    /// The requested durable state transition is illegal.
    #[error("illegal durable state transition")]
    IllegalTransition,
}

impl StoreError {
    /// Converts to the stable transport-neutral public contract without engine details.
    #[must_use]
    pub fn public(&self) -> PublicError {
        let (code, message) = match self {
            Self::NotFound => ("not_found", "resource was not found"),
            Self::AlreadyExists => ("already_exists", "resource already exists"),
            Self::GenerationConflict { .. } => {
                ("generation_conflict", "application generation is stale")
            }
            Self::SchemaMismatch => ("schema_mismatch", "database schema is incompatible"),
            Self::Corrupt => (
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
            Self::Database => ("storage_unavailable", "database operation failed"),
        };
        PublicError::new(ErrorCode::new(code), message)
    }
}

/// Current persisted application status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationState {
    Pending,
    Resolving,
    Building,
    Deploying,
    Ready,
    Degraded,
    Deleting,
    Failed,
}
impl ApplicationState {
    fn as_str(self) -> &'static str {
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
                    Self::Ready | Self::Degraded | Self::Failed,
                    Self::Pending | Self::Resolving | Self::Deleting
                )
            )
    }
}

/// Durable operation category.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Create,
    Replace,
    Delete,
    Reconcile,
    Build,
    Deploy,
}
impl OperationKind {
    fn as_str(self) -> &'static str {
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
    Pending,
    Running,
    Recovery,
    Succeeded,
    Failed,
    Cancelled,
}
impl WorkState {
    fn as_str(self) -> &'static str {
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
    Pending,
    Running,
    Recovery,
    Succeeded,
    Failed,
    Cancelled,
    Skipped,
}
impl StepState {
    fn as_str(self) -> &'static str {
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
    pub application: NormalizedApplication,
    pub resolved: Option<Value>,
    pub generation: u64,
    pub spec_hash: String,
    pub delete_intent: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}
/// Application status row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplicationStatus {
    pub application_id: ApplicationId,
    pub state: ApplicationState,
    pub observed_generation: Option<u64>,
    pub message: Option<String>,
    pub updated_at_ms: i64,
}
/// Durable operation row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Operation {
    pub id: String,
    pub application_id: ApplicationId,
    pub generation: u64,
    pub kind: OperationKind,
    pub state: WorkState,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}
/// Durable operation step row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationStep {
    pub id: String,
    pub operation_id: String,
    pub position: u32,
    pub kind: String,
    pub state: StepState,
    pub attempt: u32,
}
/// Durable build row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Build {
    pub id: String,
    pub operation_id: String,
    pub application_id: ApplicationId,
    pub service_name: String,
    pub state: WorkState,
    pub source_commit: Option<String>,
    pub image_reference: Option<String>,
    pub image_digest: Option<String>,
}
/// Result of an atomic desired-state mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutationResult {
    pub generation: u64,
    pub operation_id: String,
}

/// Application persistence contract used by later API/reconciliation layers.
#[async_trait]
pub trait ApplicationRepository: Send + Sync {
    async fn create(
        &self,
        app: &NormalizedApplication,
        resolved: Option<&Value>,
        steps: &[String],
    ) -> Result<MutationResult, StoreError>;
    async fn replace(
        &self,
        app: &NormalizedApplication,
        resolved: Option<&Value>,
        expected_generation: u64,
        steps: &[String],
    ) -> Result<MutationResult, StoreError>;
    async fn request_delete(
        &self,
        id: &ApplicationId,
        expected_generation: u64,
        steps: &[String],
    ) -> Result<MutationResult, StoreError>;
    async fn get(&self, id: &ApplicationId) -> Result<StoredApplication, StoreError>;
}
/// Operation journal persistence contract.
#[async_trait]
pub trait OperationRepository: Send + Sync {
    async fn operation(&self, id: &str) -> Result<Operation, StoreError>;
    async fn operation_steps(&self, id: &str) -> Result<Vec<OperationStep>, StoreError>;
    async fn pending_operations(&self, limit: u32) -> Result<Vec<Operation>, StoreError>;
    async fn transition_operation(
        &self,
        id: &str,
        from: WorkState,
        to: WorkState,
        error: Option<(&str, &str)>,
    ) -> Result<(), StoreError>;
    async fn transition_step(
        &self,
        id: &str,
        from: StepState,
        to: StepState,
        error: Option<(&str, &str)>,
    ) -> Result<(), StoreError>;
    async fn recover_interrupted(&self) -> Result<u64, StoreError>;
}
/// Status persistence contract.
#[async_trait]
pub trait StatusRepository: Send + Sync {
    async fn status(&self, id: &ApplicationId) -> Result<ApplicationStatus, StoreError>;
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
    async fn create_build(
        &self,
        operation_id: &str,
        application_id: &ApplicationId,
        service_name: &str,
    ) -> Result<Build, StoreError>;
    async fn build(&self, id: &str) -> Result<Build, StoreError>;
    async fn transition_build(
        &self,
        id: &str,
        from: WorkState,
        to: WorkState,
        error: Option<(&str, &str)>,
    ) -> Result<(), StoreError>;
    async fn prune_finished_before(&self, cutoff_ms: i64, limit: u32) -> Result<u64, StoreError>;
}

/// Official embedded-libSQL repository implementation.
#[derive(Clone)]
pub struct LibsqlStore {
    database: Arc<Database>,
    instance_id: String,
}

impl LibsqlStore {
    /// Opens a local embedded database, applies forward migrations, and creates or loads its instance ID.
    ///
    /// # Errors
    /// Returns a sanitized storage or schema compatibility error.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let database = Builder::new_local(path.as_ref())
            .build()
            .await
            .map_err(|_| StoreError::Database)?;
        let connection = database.connect().map_err(|_| StoreError::Database)?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")
            .await
            .map_err(|_| StoreError::Database)?;
        let mut rows = connection
            .query("PRAGMA user_version", ())
            .await
            .map_err(|_| StoreError::Database)?;
        let version: u64 = rows
            .next()
            .await
            .map_err(|_| StoreError::Database)?
            .ok_or(StoreError::Database)?
            .get(0)
            .map_err(|_| StoreError::Database)?;
        if version > SCHEMA_VERSION {
            return Err(StoreError::SchemaMismatch);
        }
        for (index, migration) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            let tx = connection
                .transaction()
                .await
                .map_err(|_| StoreError::Database)?;
            tx.execute_batch(migration)
                .await
                .map_err(|_| StoreError::Database)?;
            tx.execute_batch(&format!("PRAGMA user_version = {}", index + 1))
                .await
                .map_err(|_| StoreError::Database)?;
            tx.commit().await.map_err(|_| StoreError::Database)?;
        }
        let now = now_ms();
        let generated = format!("instance-{}", Uuid::now_v7().simple());
        connection.execute("INSERT OR IGNORE INTO instance_metadata(singleton,instance_id,schema_version,created_at_ms) VALUES(1,?1,?2,?3)",params![generated.clone(),SCHEMA_VERSION as i64,now]).await.map_err(|_|StoreError::Database)?;
        let mut rows = connection
            .query(
                "SELECT instance_id,schema_version FROM instance_metadata WHERE singleton=1",
                (),
            )
            .await
            .map_err(|_| StoreError::Database)?;
        let row = rows
            .next()
            .await
            .map_err(|_| StoreError::Database)?
            .ok_or(StoreError::Corrupt)?;
        let instance_id: String = row.get(0).map_err(|_| StoreError::Corrupt)?;
        let metadata_version: u64 = row.get(1).map_err(|_| StoreError::Corrupt)?;
        if metadata_version != SCHEMA_VERSION {
            return Err(StoreError::SchemaMismatch);
        }
        Ok(Self {
            database: Arc::new(database),
            instance_id,
        })
    }
    /// Stable identity of this control-plane database.
    #[must_use]
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }
    fn connection(&self) -> Result<Connection, StoreError> {
        self.database.connect().map_err(|_| StoreError::Database)
    }
    /// Inserts logical-secret metadata for existence validation. Encryption lifecycle arrives in Plan 07.
    ///
    /// # Errors
    /// Returns `AlreadyExists` for duplicate names or a sanitized storage error.
    pub async fn declare_logical_secret(&self, name: &str) -> Result<(), StoreError> {
        let c = self.connection()?;
        let now = now_ms();
        c.execute("INSERT INTO logical_secrets(id,name,generation,value_is_set,created_at_ms,updated_at_ms) VALUES(?1,?2,1,0,?3,?3)",params![new_id("secret"),name,now]).await.map_err(|_|StoreError::AlreadyExists)?;
        Ok(())
    }
    async fn validate_secrets(
        &self,
        tx: &Transaction,
        app: &NormalizedApplication,
    ) -> Result<(), StoreError> {
        let names: Vec<String> = app
            .spec
            .services
            .iter()
            .flat_map(|s| s.secrets.iter().map(|v| v.source.clone()))
            .collect();
        let mut missing = Vec::new();
        for name in names {
            let mut rows = tx
                .query(
                    "SELECT 1 FROM logical_secrets WHERE name=?1",
                    params![name.clone()],
                )
                .await
                .map_err(|_| StoreError::Database)?;
            if rows
                .next()
                .await
                .map_err(|_| StoreError::Database)?
                .is_none()
            {
                missing.push(name);
            }
        }
        missing.sort();
        missing.dedup();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(StoreError::MissingSecrets(missing))
        }
    }
    async fn insert_operation(
        tx: &Transaction,
        app_id: &ApplicationId,
        generation: u64,
        kind: OperationKind,
        steps: &[String],
        now: i64,
    ) -> Result<String, StoreError> {
        let id = new_id("operation");
        tx.execute("INSERT INTO operations(id,application_id,generation,kind,state,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,'pending',?5,?5)",params![id.clone(),app_id.as_str(),generation as i64,kind.as_str(),now]).await.map_err(|_|StoreError::Database)?;
        for (position, kind) in steps.iter().enumerate() {
            tx.execute("INSERT INTO operation_steps(id,operation_id,position,kind,state,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,'pending',?5,?5)",params![new_id("step"),id.clone(),position as i64,kind.clone(),now]).await.map_err(|_|StoreError::Database)?;
        }
        Ok(id)
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::now_v7().simple())
}
fn canonical_resolved(value: Option<&Value>) -> Result<Option<String>, StoreError> {
    value
        .map(|v| serde_json::to_string(v).map_err(|_| StoreError::Corrupt))
        .transpose()
}
fn decode_application(
    json: &str,
    expected_hash: &str,
) -> Result<NormalizedApplication, StoreError> {
    let raw: NormalizedApplication = serde_json::from_str(json).map_err(|_| StoreError::Corrupt)?;
    // Round-trip through the strict public parser to restore all semantic invariants.
    let toml = raw.export_toml().map_err(|_| StoreError::Corrupt)?;
    let validated = piqueld_core::parse_toml(&toml).map_err(|_| StoreError::Corrupt)?;
    let app = validated.normalize(raw.id.clone());
    if app.spec_hash() != expected_hash
        || app.canonical_json().map_err(|_| StoreError::Corrupt)? != json
    {
        return Err(StoreError::Corrupt);
    }
    Ok(app)
}

#[async_trait]
impl ApplicationRepository for LibsqlStore {
    async fn create(
        &self,
        app: &NormalizedApplication,
        resolved: Option<&Value>,
        steps: &[String],
    ) -> Result<MutationResult, StoreError> {
        let c = self.connection()?;
        let tx = c.transaction().await.map_err(|_| StoreError::Database)?;
        self.validate_secrets(&tx, app).await?;
        let desired = app.canonical_json().map_err(|_| StoreError::Corrupt)?;
        let resolved = canonical_resolved(resolved)?;
        let hash = app.spec_hash();
        let now = now_ms();
        let inserted=tx.execute("INSERT OR IGNORE INTO applications(id,name,generation,desired_json,resolved_json,spec_hash,created_at_ms,updated_at_ms) VALUES(?1,?2,1,?3,?4,?5,?6,?6)",params![app.id.as_str(),app.metadata.name.as_str(),desired,resolved,hash,now]).await.map_err(|_|StoreError::Database)?;
        if inserted != 1 {
            return Err(StoreError::AlreadyExists);
        }
        tx.execute("INSERT INTO application_status(application_id,state,updated_at_ms) VALUES(?1,'pending',?2)",params![app.id.as_str(),now]).await.map_err(|_|StoreError::Database)?;
        let operation_id =
            Self::insert_operation(&tx, &app.id, 1, OperationKind::Create, steps, now).await?;
        tx.commit().await.map_err(|_| StoreError::Database)?;
        Ok(MutationResult {
            generation: 1,
            operation_id,
        })
    }
    async fn replace(
        &self,
        app: &NormalizedApplication,
        resolved: Option<&Value>,
        expected_generation: u64,
        steps: &[String],
    ) -> Result<MutationResult, StoreError> {
        let c = self.connection()?;
        let tx = c.transaction().await.map_err(|_| StoreError::Database)?;
        self.validate_secrets(&tx, app).await?;
        let generation = expected_generation
            .checked_add(1)
            .ok_or(StoreError::Corrupt)?;
        let desired = app.canonical_json().map_err(|_| StoreError::Corrupt)?;
        let resolved = canonical_resolved(resolved)?;
        let hash = app.spec_hash();
        let now = now_ms();
        let changed=tx.execute("UPDATE OR IGNORE applications SET name=?1,generation=?2,desired_json=?3,resolved_json=?4,spec_hash=?5,delete_intent=0,updated_at_ms=?6 WHERE id=?7 AND generation=?8",params![app.metadata.name.as_str(),generation as i64,desired,resolved,hash,now,app.id.as_str(),expected_generation as i64]).await.map_err(|_|StoreError::Database)?;
        if changed != 1 {
            let mut rows = tx
                .query(
                    "SELECT generation FROM applications WHERE id=?1",
                    params![app.id.as_str()],
                )
                .await
                .map_err(|_| StoreError::Database)?;
            let row = rows
                .next()
                .await
                .map_err(|_| StoreError::Database)?
                .ok_or(StoreError::NotFound)?;
            let actual: u64 = row.get(0).map_err(|_| StoreError::Corrupt)?;
            return if actual == expected_generation {
                Err(StoreError::AlreadyExists)
            } else {
                Err(StoreError::GenerationConflict {
                    expected: expected_generation,
                    actual,
                })
            };
        }
        tx.execute("UPDATE application_status SET state='pending',observed_generation=NULL,message=NULL,updated_at_ms=?1 WHERE application_id=?2",params![now,app.id.as_str()]).await.map_err(|_|StoreError::Database)?;
        let operation_id =
            Self::insert_operation(&tx, &app.id, generation, OperationKind::Replace, steps, now)
                .await?;
        tx.commit().await.map_err(|_| StoreError::Database)?;
        Ok(MutationResult {
            generation,
            operation_id,
        })
    }
    async fn request_delete(
        &self,
        id: &ApplicationId,
        expected_generation: u64,
        steps: &[String],
    ) -> Result<MutationResult, StoreError> {
        let c = self.connection()?;
        let tx = c.transaction().await.map_err(|_| StoreError::Database)?;
        let generation = expected_generation
            .checked_add(1)
            .ok_or(StoreError::Corrupt)?;
        let now = now_ms();
        let changed=tx.execute("UPDATE applications SET generation=?1,delete_intent=1,updated_at_ms=?2 WHERE id=?3 AND generation=?4",params![generation as i64,now,id.as_str(),expected_generation as i64]).await.map_err(|_|StoreError::Database)?;
        if changed != 1 {
            let mut rows = tx
                .query(
                    "SELECT generation FROM applications WHERE id=?1",
                    params![id.as_str()],
                )
                .await
                .map_err(|_| StoreError::Database)?;
            let row = rows
                .next()
                .await
                .map_err(|_| StoreError::Database)?
                .ok_or(StoreError::NotFound)?;
            let actual = row.get(0).map_err(|_| StoreError::Corrupt)?;
            return Err(StoreError::GenerationConflict {
                expected: expected_generation,
                actual,
            });
        }
        tx.execute("UPDATE application_status SET state='deleting',message=NULL,updated_at_ms=?1 WHERE application_id=?2",params![now,id.as_str()]).await.map_err(|_|StoreError::Database)?;
        let operation_id =
            Self::insert_operation(&tx, id, generation, OperationKind::Delete, steps, now).await?;
        tx.commit().await.map_err(|_| StoreError::Database)?;
        Ok(MutationResult {
            generation,
            operation_id,
        })
    }
    async fn get(&self, id: &ApplicationId) -> Result<StoredApplication, StoreError> {
        let c = self.connection()?;
        let mut rows=c.query("SELECT desired_json,resolved_json,generation,spec_hash,delete_intent,created_at_ms,updated_at_ms FROM applications WHERE id=?1",params![id.as_str()]).await.map_err(|_|StoreError::Database)?;
        let row = rows
            .next()
            .await
            .map_err(|_| StoreError::Database)?
            .ok_or(StoreError::NotFound)?;
        let desired: String = row.get(0).map_err(|_| StoreError::Corrupt)?;
        let resolved_json: Option<String> = row.get(1).map_err(|_| StoreError::Corrupt)?;
        let generation: u64 = row.get(2).map_err(|_| StoreError::Corrupt)?;
        let spec_hash: String = row.get(3).map_err(|_| StoreError::Corrupt)?;
        let delete: i64 = row.get(4).map_err(|_| StoreError::Corrupt)?;
        let created_at_ms = row.get(5).map_err(|_| StoreError::Corrupt)?;
        let updated_at_ms = row.get(6).map_err(|_| StoreError::Corrupt)?;
        let application = decode_application(&desired, &spec_hash)?;
        let resolved = resolved_json
            .map(|v| serde_json::from_str(&v).map_err(|_| StoreError::Corrupt))
            .transpose()?;
        Ok(StoredApplication {
            application,
            resolved,
            generation,
            spec_hash,
            delete_intent: delete == 1,
            created_at_ms,
            updated_at_ms,
        })
    }
}

fn parse_operation_row(row: &libsql::Row) -> Result<Operation, StoreError> {
    Ok(Operation {
        id: row.get(0).map_err(|_| StoreError::Corrupt)?,
        application_id: ApplicationId::parse(
            row.get::<String>(1).map_err(|_| StoreError::Corrupt)?,
        )
        .map_err(|_| StoreError::Corrupt)?,
        generation: row.get(2).map_err(|_| StoreError::Corrupt)?,
        kind: OperationKind::parse(&row.get::<String>(3).map_err(|_| StoreError::Corrupt)?)?,
        state: WorkState::parse(&row.get::<String>(4).map_err(|_| StoreError::Corrupt)?)?,
        error_code: row.get(5).map_err(|_| StoreError::Corrupt)?,
        error_message: row.get(6).map_err(|_| StoreError::Corrupt)?,
        created_at_ms: row.get(7).map_err(|_| StoreError::Corrupt)?,
        updated_at_ms: row.get(8).map_err(|_| StoreError::Corrupt)?,
    })
}

#[async_trait]
impl OperationRepository for LibsqlStore {
    async fn operation(&self, id: &str) -> Result<Operation, StoreError> {
        let c = self.connection()?;
        let mut rows=c.query("SELECT id,application_id,generation,kind,state,error_code,error_message,created_at_ms,updated_at_ms FROM operations WHERE id=?1",params![id]).await.map_err(|_|StoreError::Database)?;
        let row = rows
            .next()
            .await
            .map_err(|_| StoreError::Database)?
            .ok_or(StoreError::NotFound)?;
        parse_operation_row(&row)
    }
    async fn operation_steps(&self, id: &str) -> Result<Vec<OperationStep>, StoreError> {
        let c = self.connection()?;
        let mut rows=c.query("SELECT id,operation_id,position,kind,state,attempt FROM operation_steps WHERE operation_id=?1 ORDER BY position",params![id]).await.map_err(|_|StoreError::Database)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(|_| StoreError::Database)? {
            out.push(OperationStep {
                id: row.get(0).map_err(|_| StoreError::Corrupt)?,
                operation_id: row.get(1).map_err(|_| StoreError::Corrupt)?,
                position: row.get(2).map_err(|_| StoreError::Corrupt)?,
                kind: row.get(3).map_err(|_| StoreError::Corrupt)?,
                state: match row
                    .get::<String>(4)
                    .map_err(|_| StoreError::Corrupt)?
                    .as_str()
                {
                    "pending" => StepState::Pending,
                    "running" => StepState::Running,
                    "recovery" => StepState::Recovery,
                    "succeeded" => StepState::Succeeded,
                    "failed" => StepState::Failed,
                    "cancelled" => StepState::Cancelled,
                    "skipped" => StepState::Skipped,
                    _ => return Err(StoreError::Corrupt),
                },
                attempt: row.get(5).map_err(|_| StoreError::Corrupt)?,
            });
        }
        Ok(out)
    }
    async fn pending_operations(&self, limit: u32) -> Result<Vec<Operation>, StoreError> {
        let c = self.connection()?;
        let mut rows=c.query("SELECT id,application_id,generation,kind,state,error_code,error_message,created_at_ms,updated_at_ms FROM operations WHERE state IN ('pending','recovery') ORDER BY created_at_ms,id LIMIT ?1",params![i64::from(limit)]).await.map_err(|_|StoreError::Database)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(|_| StoreError::Database)? {
            out.push(parse_operation_row(&row)?);
        }
        Ok(out)
    }
    async fn transition_operation(
        &self,
        id: &str,
        from: WorkState,
        to: WorkState,
        error: Option<(&str, &str)>,
    ) -> Result<(), StoreError> {
        if !from.can_transition_to(to) {
            return Err(StoreError::IllegalTransition);
        }
        let c = self.connection()?;
        let now = now_ms();
        let (error_code, error_message) = error.map_or((None, None), |(a, b)| (Some(a), Some(b)));
        let finished = to.terminal().then_some(now);
        let started = (to == WorkState::Running).then_some(now);
        let changed=c.execute("UPDATE operations SET state=?1,error_code=?2,error_message=?3,updated_at_ms=?4,started_at_ms=COALESCE(started_at_ms,?5),finished_at_ms=?6 WHERE id=?7 AND state=?8",params![to.as_str(),error_code,error_message,now,started,finished,id,from.as_str()]).await.map_err(|_|StoreError::Database)?;
        if changed == 1 {
            Ok(())
        } else {
            Err(StoreError::IllegalTransition)
        }
    }
    async fn transition_step(
        &self,
        id: &str,
        from: StepState,
        to: StepState,
        error: Option<(&str, &str)>,
    ) -> Result<(), StoreError> {
        if !from.can_transition_to(to) {
            return Err(StoreError::IllegalTransition);
        }
        let c = self.connection()?;
        let now = now_ms();
        let (error_code, error_message) = error.map_or((None, None), |(a, b)| (Some(a), Some(b)));
        let finished = to.terminal().then_some(now);
        let started = (to == StepState::Running).then_some(now);
        let attempt = i32::from(to == StepState::Running);
        let changed=c.execute("UPDATE operation_steps SET state=?1,error_code=?2,error_message=?3,updated_at_ms=?4,started_at_ms=COALESCE(started_at_ms,?5),finished_at_ms=?6,attempt=attempt+?7 WHERE id=?8 AND state=?9",params![to.as_str(),error_code,error_message,now,started,finished,attempt,id,from.as_str()]).await.map_err(|_|StoreError::Database)?;
        if changed == 1 {
            Ok(())
        } else {
            Err(StoreError::IllegalTransition)
        }
    }
    async fn recover_interrupted(&self) -> Result<u64, StoreError> {
        let c = self.connection()?;
        let tx = c.transaction().await.map_err(|_| StoreError::Database)?;
        let now = now_ms();
        let mut count=tx.execute("UPDATE operations SET state='recovery',updated_at_ms=?1,started_at_ms=NULL WHERE state='running'",params![now]).await.map_err(|_|StoreError::Database)?;
        count+=tx.execute("UPDATE operation_steps SET state='recovery',updated_at_ms=?1,started_at_ms=NULL WHERE state='running'",params![now]).await.map_err(|_|StoreError::Database)?;
        count+=tx.execute("UPDATE builds SET state='recovery',updated_at_ms=?1,started_at_ms=NULL WHERE state='running'",params![now]).await.map_err(|_|StoreError::Database)?;
        tx.commit().await.map_err(|_| StoreError::Database)?;
        Ok(count)
    }
}

#[async_trait]
impl StatusRepository for LibsqlStore {
    async fn status(&self, id: &ApplicationId) -> Result<ApplicationStatus, StoreError> {
        let c = self.connection()?;
        let mut rows=c.query("SELECT state,observed_generation,message,updated_at_ms FROM application_status WHERE application_id=?1",params![id.as_str()]).await.map_err(|_|StoreError::Database)?;
        let row = rows
            .next()
            .await
            .map_err(|_| StoreError::Database)?
            .ok_or(StoreError::NotFound)?;
        Ok(ApplicationStatus {
            application_id: id.clone(),
            state: ApplicationState::parse(
                &row.get::<String>(0).map_err(|_| StoreError::Corrupt)?,
            )?,
            observed_generation: row.get(1).map_err(|_| StoreError::Corrupt)?,
            message: row.get(2).map_err(|_| StoreError::Corrupt)?,
            updated_at_ms: row.get(3).map_err(|_| StoreError::Corrupt)?,
        })
    }
    async fn set_status(
        &self,
        id: &ApplicationId,
        from: ApplicationState,
        to: ApplicationState,
        observed_generation: Option<u64>,
        message: Option<&str>,
    ) -> Result<(), StoreError> {
        if !from.can_transition_to(to) {
            return Err(StoreError::IllegalTransition);
        }
        let c = self.connection()?;
        let changed=c.execute("UPDATE application_status SET state=?1,observed_generation=?2,message=?3,updated_at_ms=?4 WHERE application_id=?5 AND state=?6",params![to.as_str(),observed_generation.map(|v|v as i64),message,now_ms(),id.as_str(),from.as_str()]).await.map_err(|_|StoreError::Database)?;
        if changed == 1 {
            Ok(())
        } else {
            Err(StoreError::IllegalTransition)
        }
    }
}

#[async_trait]
impl BuildRepository for LibsqlStore {
    async fn create_build(
        &self,
        operation_id: &str,
        application_id: &ApplicationId,
        service_name: &str,
    ) -> Result<Build, StoreError> {
        let c = self.connection()?;
        let id = new_id("build");
        let now = now_ms();
        c.execute("INSERT INTO builds(id,operation_id,application_id,service_name,state,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,'pending',?5,?5)",params![id.clone(),operation_id,application_id.as_str(),service_name,now]).await.map_err(|_|StoreError::Database)?;
        Ok(Build {
            id,
            operation_id: operation_id.into(),
            application_id: application_id.clone(),
            service_name: service_name.into(),
            state: WorkState::Pending,
            source_commit: None,
            image_reference: None,
            image_digest: None,
        })
    }
    async fn build(&self, id: &str) -> Result<Build, StoreError> {
        let c = self.connection()?;
        let mut rows=c.query("SELECT id,operation_id,application_id,service_name,state,source_commit,image_reference,image_digest FROM builds WHERE id=?1",params![id]).await.map_err(|_|StoreError::Database)?;
        let row = rows
            .next()
            .await
            .map_err(|_| StoreError::Database)?
            .ok_or(StoreError::NotFound)?;
        Ok(Build {
            id: row.get(0).map_err(|_| StoreError::Corrupt)?,
            operation_id: row.get(1).map_err(|_| StoreError::Corrupt)?,
            application_id: ApplicationId::parse(
                row.get::<String>(2).map_err(|_| StoreError::Corrupt)?,
            )
            .map_err(|_| StoreError::Corrupt)?,
            service_name: row.get(3).map_err(|_| StoreError::Corrupt)?,
            state: WorkState::parse(&row.get::<String>(4).map_err(|_| StoreError::Corrupt)?)?,
            source_commit: row.get(5).map_err(|_| StoreError::Corrupt)?,
            image_reference: row.get(6).map_err(|_| StoreError::Corrupt)?,
            image_digest: row.get(7).map_err(|_| StoreError::Corrupt)?,
        })
    }
    async fn transition_build(
        &self,
        id: &str,
        from: WorkState,
        to: WorkState,
        error: Option<(&str, &str)>,
    ) -> Result<(), StoreError> {
        if !from.can_transition_to(to) {
            return Err(StoreError::IllegalTransition);
        }
        let c = self.connection()?;
        let now = now_ms();
        let (ec, em) = error.map_or((None, None), |(a, b)| (Some(a), Some(b)));
        let changed=c.execute("UPDATE builds SET state=?1,error_code=?2,error_message=?3,updated_at_ms=?4,started_at_ms=COALESCE(started_at_ms,?5),finished_at_ms=?6 WHERE id=?7 AND state=?8",params![to.as_str(),ec,em,now,(to==WorkState::Running).then_some(now),to.terminal().then_some(now),id,from.as_str()]).await.map_err(|_|StoreError::Database)?;
        if changed == 1 {
            Ok(())
        } else {
            Err(StoreError::IllegalTransition)
        }
    }
    async fn prune_finished_before(&self, cutoff_ms: i64, limit: u32) -> Result<u64, StoreError> {
        let c = self.connection()?;
        c.execute("DELETE FROM builds WHERE id IN (SELECT id FROM builds WHERE finished_at_ms IS NOT NULL AND finished_at_ms < ?1 ORDER BY finished_at_ms LIMIT ?2)",params![cutoff_ms,i64::from(limit)]).await.map_err(|_|StoreError::Database)
    }
}
