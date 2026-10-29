//! Stopped environments, whose services run no replicas and whose routes
//! the gateway doesn't serve, until they are started or deployed again.
use super::{Store, StoreError, acceptance::Accepted};
use piqueld_core::{EnvironmentId, Operation};
use sqlx::{Sqlite, Transaction};

impl Store {
    /// Whether environment `id` is stopped.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn is_stopped(&self, id: &EnvironmentId) -> Result<bool, StoreError> {
        let id = id.as_str();
        let stopped = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "stopped!: i64" FROM environments WHERE id=?1 AND stopped=1"#,
            id
        )
        .fetch_one(&self.pool)
        .await
        .map_err(StoreError::database)?;
        Ok(stopped > 0)
    }

    /// Stops environment `id` and responds with its latest operation. One
    /// whose deployment has rolled out runs again if it has ended or failed,
    /// so its services scale to zero now; a deployment still in progress
    /// rolls out with no replicas. Earlier deployments that never rolled out
    /// are not retried: active-target repair scales their services down.
    /// `NotFound` before its first deployment, `Busy` while it is deleted.
    pub(super) async fn stop_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: &EnvironmentId,
        expected: Option<u64>,
        now: i64,
    ) -> Result<Accepted, StoreError> {
        let operation = Self::latest_deployment_on(tx, id, expected).await?;
        Self::mark_stopped_on(tx, id.as_str(), true, now).await?;
        let row = operation.id.as_str();
        let promoted = sqlx::query_scalar!("SELECT promoted FROM operations WHERE id=?1", row)
            .fetch_one(&mut **tx)
            .await
            .map_err(StoreError::database)?;
        let operation = if promoted != 0 {
            Self::retry_operation_on(tx, &operation).await?
        } else {
            operation
        };
        Ok(Self::operation_accepted(&operation))
    }

    /// Starts environment `id` and reconciles it: its latest operation runs
    /// again with its saved inputs once it has ended or failed, or continues
    /// while still in progress. Responds with that operation; `NotFound`
    /// before its first deployment.
    pub(super) async fn start_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: &EnvironmentId,
        expected: Option<u64>,
        now: i64,
    ) -> Result<Accepted, StoreError> {
        let environment = Self::checked_environment_on(tx, id, expected).await?;
        let operation = Self::latest_operation_on(tx, id)
            .await?
            .ok_or(StoreError::NotFound)?;
        if !environment.delete_intent() {
            Self::mark_stopped_on(tx, id.as_str(), false, now).await?;
        }
        let operation = if operation.state.terminal() || operation.error_code.is_some() {
            Self::retry_operation_on(tx, &operation).await?
        } else {
            operation
        };
        Ok(Self::operation_accepted(&operation))
    }

    /// The latest operation of environment `id`, which deploys it: `Busy`
    /// while it is deleted, `NotFound` before its first deployment.
    async fn latest_deployment_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: &EnvironmentId,
        expected: Option<u64>,
    ) -> Result<Operation, StoreError> {
        if Self::checked_environment_on(tx, id, expected)
            .await?
            .delete_intent()
        {
            return Err(StoreError::Busy);
        }
        Self::latest_operation_on(tx, id)
            .await?
            .ok_or(StoreError::NotFound)
    }

    /// Marks environment `id` stopped or started, recording
    /// `environment_stopped` or `environment_started` when that changes it.
    pub(super) async fn mark_stopped_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: &str,
        stopped: bool,
        now: i64,
    ) -> Result<(), StoreError> {
        let value = i64::from(stopped);
        let changed = sqlx::query!(
            "UPDATE environments SET stopped=?1,updated_at_ms=?2 WHERE id=?3 AND stopped!=?1",
            value,
            now,
            id
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        if changed == 0 {
            return Ok(());
        }
        let name = sqlx::query_scalar!("SELECT name FROM environments WHERE id=?1", id)
            .fetch_one(&mut **tx)
            .await
            .map_err(StoreError::database)?;
        let (kind, message) = if stopped {
            (
                "environment_stopped",
                format!(
                    "environment {name} is stopped: its services scale to zero and its routes are withdrawn"
                ),
            )
        } else {
            (
                "environment_started",
                format!("environment {name} is started"),
            )
        };
        Self::environment_event_on(tx, id, kind, &message, now).await
    }
}
