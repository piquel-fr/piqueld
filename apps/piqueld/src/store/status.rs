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
            "SELECT state,message,updated_at_ms FROM application_status WHERE application_id=?1",
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
        let changed = sqlx::query!("UPDATE application_status SET state=?1,message=?2,updated_at_ms=?3 WHERE application_id=(SELECT application_id FROM operations WHERE id=?4) AND ?4=(SELECT latest.id FROM operations latest WHERE latest.application_id=application_status.application_id ORDER BY latest.created_at_ms DESC,latest.id DESC LIMIT 1)",state,message,now,operation_id)
            .execute(&self.pool).await.map_err(StoreError::database)?.rows_affected();
        Ok(changed == 1)
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
