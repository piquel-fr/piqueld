//! Runtime status writes are guarded by the operation that observed them.

use super::{ApplicationId, ApplicationState, ApplicationStatus, SqliteStore, StoreError, now_ms};
use serde::Deserialize;
use sqlx::{Sqlite, SqliteConnection, Transaction};

impl SqliteStore {
    /// Reads the application's last observed status.
    ///
    /// # Errors
    /// Returns a storage error or `NotFound`.
    pub async fn status(&self, id: &ApplicationId) -> Result<ApplicationStatus, StoreError> {
        Self::status_on(
            &mut *self.pool.acquire().await.map_err(StoreError::database)?,
            id,
        )
        .await
    }

    pub(super) async fn status_on(
        connection: &mut SqliteConnection,
        id: &ApplicationId,
    ) -> Result<ApplicationStatus, StoreError> {
        let app_id = id.as_str();
        let row = sqlx::query!(
            "SELECT state,message,runtime_health,updated_at_ms FROM application_status WHERE application_id=?1",
            app_id
        )
        .fetch_optional(connection)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        Ok(ApplicationStatus {
            application_id: id.clone(),
            state: ApplicationState::deserialize(serde::de::value::StrDeserializer::<
                serde::de::value::Error,
            >::new(&row.state))
            .map_err(StoreError::corrupt)?,
            runtime_health: row.runtime_health,
            message: row.message,
            updated_at_ms: row.updated_at_ms,
        })
    }

    /// Updates status only for the application's latest operation.
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
        let changed = sqlx::query!("UPDATE application_status SET state=?1,message=?2,updated_at_ms=?3 WHERE (state!=?1 OR message IS NOT ?2) AND application_id=(SELECT application_id FROM operations WHERE id=?4) AND ?4=(SELECT latest.id FROM operations latest WHERE latest.application_id=application_status.application_id ORDER BY latest.created_at_ms DESC,latest.id DESC LIMIT 1)",state,message,now,operation_id)
            .execute(&mut *tx).await.map_err(StoreError::database)?.rows_affected();
        if changed == 1 {
            Self::operation_event(&mut tx, operation_id, "status_changed", Some(state), now)
                .await?;
        }
        let current=sqlx::query_scalar!("SELECT id FROM operations WHERE application_id=(SELECT application_id FROM operations WHERE id=?1) ORDER BY created_at_ms DESC,id DESC LIMIT 1",operation_id).fetch_optional(&mut *tx).await.map_err(StoreError::database)?.flatten();
        tx.commit().await.map_err(StoreError::database)?;
        Ok(current.as_deref() == Some(operation_id))
    }

    pub(super) async fn write_status(
        tx: &mut Transaction<'_, Sqlite>,
        app_id: &str,
        state: &str,
        message: Option<&str>,
        now: i64,
    ) -> Result<(), StoreError> {
        sqlx::query!("INSERT INTO application_status(application_id,state,message,updated_at_ms) VALUES(?1,?2,?3,?4) ON CONFLICT(application_id) DO UPDATE SET state=excluded.state,message=excluded.message,updated_at_ms=excluded.updated_at_ms",app_id,state,message,now)
            .execute(&mut **tx).await.map_err(StoreError::database)?;
        Ok(())
    }
}

impl SqliteStore {
    /// Records a meaningful observed health transition without overwriting intent progress.
    /// # Errors
    /// Returns a store error. Superseded observations are ignored.
    pub async fn record_health(
        &self,
        operation_id: &str,
        observed: &piqueld_core::ObservedApplication,
    ) -> Result<(), StoreError> {
        let health = if observed.services.is_empty() {
            "absent"
        } else if observed
            .services
            .iter()
            .all(|service| service.convergence == piqueld_core::Convergence::Converged)
        {
            "ready"
        } else {
            "degraded"
        };
        let (_writer, mut tx) = self.begin_immediate().await?;
        let now = now_ms();
        let changed=sqlx::query!("UPDATE application_status SET runtime_health=?1 WHERE runtime_health IS NOT ?1 AND application_id=(SELECT application_id FROM operations WHERE id=?2) AND ?2=(SELECT latest.id FROM operations latest WHERE latest.application_id=application_status.application_id ORDER BY latest.created_at_ms DESC,latest.id DESC LIMIT 1)",health,operation_id).execute(&mut *tx).await.map_err(StoreError::database)?.rows_affected();
        if changed == 1 {
            sqlx::query!("INSERT INTO events(application_id,operation_id,generation,kind,message,created_at_ms) SELECT application_id,id,generation,'health_changed',?1,?2 FROM operations WHERE id=?3",health,now,operation_id).execute(&mut *tx).await.map_err(StoreError::database)?;
        }
        tx.commit().await.map_err(StoreError::database)
    }
}
