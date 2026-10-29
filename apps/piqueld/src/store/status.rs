//! Runtime status writes are guarded by the operation that observed them.

use super::{
    ApplicationState, EnvironmentId, EnvironmentStatus, Store, StoreError, decode_name, now_ms,
};
use sqlx::{Sqlite, SqliteConnection, Transaction};

impl Store {
    /// Reads the environment's last observed status.
    ///
    /// # Errors
    /// Returns a storage error or `NotFound`.
    pub async fn status(&self, id: &EnvironmentId) -> Result<EnvironmentStatus, StoreError> {
        Self::status_on(&mut *self.connection().await?, id).await
    }

    /// Reads status on an existing connection or transaction.
    pub(super) async fn status_on(
        connection: &mut SqliteConnection,
        id: &EnvironmentId,
    ) -> Result<EnvironmentStatus, StoreError> {
        let app_id = id.as_str();
        let row = sqlx::query!(
            "SELECT state,message,runtime_health,health_observed_at_ms,updated_at_ms FROM environment_status WHERE environment_id=?1",
            app_id
        )
        .fetch_optional(connection)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        Ok(EnvironmentStatus {
            environment_id: id.clone(),
            state: decode_name(&row.state)?,
            runtime_health: row.runtime_health,
            health_observed_at_ms: row.health_observed_at_ms,
            message: row.message,
            updated_at_ms: row.updated_at_ms,
        })
    }

    /// Updates status only for the environment's latest operation.
    /// Records `status_changed` only when the state or message actually changes.
    /// Returns whether `operation_id` is still the latest operation, so callers
    /// can stop superseded work.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn set_status_for_operation(
        &self,
        operation_id: &str,
        state: ApplicationState,
        message: Option<&str>,
    ) -> Result<bool, StoreError> {
        let now = now_ms();
        let state = state.as_str();
        let (_writer, mut tx) = self.begin_immediate().await?;
        let changed = sqlx::query!("UPDATE environment_status SET state=?1,message=?2,updated_at_ms=?3 WHERE (state!=?1 OR message IS NOT ?2) AND environment_id=(SELECT environment_id FROM operations WHERE id=?4) AND ?4=(SELECT latest.id FROM operations latest WHERE latest.environment_id=environment_status.environment_id ORDER BY latest.created_at_ms DESC,latest.id DESC LIMIT 1)",state,message,now,operation_id)
            .execute(&mut *tx).await.map_err(StoreError::database)?.rows_affected();
        if changed == 1 {
            Self::operation_event(&mut tx, operation_id, "status_changed", Some(state), now)
                .await?;
        }
        let current=sqlx::query_scalar!("SELECT id FROM operations WHERE environment_id=(SELECT environment_id FROM operations WHERE id=?1) ORDER BY created_at_ms DESC,id DESC LIMIT 1",operation_id).fetch_optional(&mut *tx).await.map_err(StoreError::database)?.flatten();
        tx.commit().await.map_err(StoreError::database)?;
        Ok(current.as_deref() == Some(operation_id))
    }

    /// Unconditionally upserts an environment's status within `tx`, clearing
    /// its message; callers are responsible for operation guards.
    pub(super) async fn write_status(
        tx: &mut Transaction<'_, Sqlite>,
        id: &str,
        state: ApplicationState,
        now: i64,
    ) -> Result<(), StoreError> {
        let state = state.as_str();
        sqlx::query!("INSERT INTO environment_status(environment_id,state,message,updated_at_ms) VALUES(?1,?2,NULL,?3) ON CONFLICT(environment_id) DO UPDATE SET state=excluded.state,message=excluded.message,updated_at_ms=excluded.updated_at_ms",id,state,now)
            .execute(&mut **tx).await.map_err(StoreError::database)?;
        Ok(())
    }

    /// Records a meaningful observed health transition without overwriting intent progress.
    /// Health is `absent` with no services, `ready` when every expected
    /// service is observed and all observed ones converged, `stopped`
    /// instead in a stopped environment once they run no replica and no
    /// task, else `degraded`.
    /// Expected are the services of the operation's target while it is in
    /// progress, else of the environment's active target, and none while it
    /// is deleted. The observation time is refreshed on every call, while a
    /// `health_changed` event is recorded only when the value changes.
    /// # Errors
    /// Returns a store error. Superseded observations are ignored.
    pub async fn record_health(
        &self,
        operation_id: &str,
        observed: &piqueld_core::ObservedApplication,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let expected = sqlx::query_scalar!(
            r#"SELECT json_extract(service.value,'$.name') AS "name!: String"
            FROM operations o JOIN environments e ON e.id=o.environment_id,
            json_each(CASE WHEN o.kind='delete' THEN NULL
            WHEN o.state IN ('requested','running') AND o.target_json IS NOT NULL THEN o.target_json
            ELSE e.resolved_json END,'$.services') service
            WHERE o.id=?1"#,
            operation_id
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let stopped = sqlx::query_scalar!(
            r#"SELECT e.stopped AS "stopped!: bool" FROM operations o JOIN environments e ON e.id=o.environment_id WHERE o.id=?1"#,
            operation_id
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .unwrap_or_default();
        let health = if observed.services.is_empty() {
            "absent"
        } else if expected.iter().all(|name| {
            observed
                .services
                .iter()
                .any(|service| &service.name == name)
        }) && observed
            .services
            .iter()
            .all(|service| service.convergence == piqueld_core::Convergence::Converged)
        {
            // Stopped once Swarm runs nothing, not merely once the flag is set.
            let down = observed.services.iter().all(|service| {
                service.replicas == 0 && service.tasks.iter().all(|task| !task.state.is_live())
            });
            if stopped && down { "stopped" } else { "ready" }
        } else {
            "degraded"
        };
        let now = now_ms();
        let changed=sqlx::query!("UPDATE environment_status SET runtime_health=?1 WHERE runtime_health IS NOT ?1 AND environment_id=(SELECT environment_id FROM operations WHERE id=?2) AND ?2=(SELECT latest.id FROM operations latest WHERE latest.environment_id=environment_status.environment_id ORDER BY latest.created_at_ms DESC,latest.id DESC LIMIT 1)",health,operation_id).execute(&mut *tx).await.map_err(StoreError::database)?.rows_affected();
        sqlx::query!("UPDATE environment_status SET health_observed_at_ms=?1 WHERE environment_id=(SELECT environment_id FROM operations WHERE id=?2) AND ?2=(SELECT id FROM operations WHERE environment_id=environment_status.environment_id ORDER BY created_at_ms DESC,id DESC LIMIT 1)",now,operation_id).execute(&mut *tx).await.map_err(StoreError::database)?;
        if changed == 1 {
            sqlx::query!("INSERT INTO events(application_id,environment_id,operation_id,generation,kind,message,created_at_ms) SELECT (SELECT application_id FROM environments WHERE id=operations.environment_id),environment_id,id,generation,'health_changed',?1,?2 FROM operations WHERE id=?3",health,now,operation_id).execute(&mut *tx).await.map_err(StoreError::database)?;
        }
        tx.commit().await.map_err(StoreError::database)
    }
}
