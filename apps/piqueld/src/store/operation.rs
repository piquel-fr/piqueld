//! Durable operation records; execution policy belongs to the controller.

use super::{
    ApplicationId, Operation, OperationKind, OperationState, SqliteStore, StoreError, new_id,
    now_ms,
};
use serde::Deserialize;
use sqlx::{Sqlite, SqliteConnection, Transaction};

struct OperationRow {
    id: String,
    application_id: String,
    kind: String,
    state: String,
    error_code: Option<String>,
    error_message: Option<String>,
    created_at_ms: i64,
    updated_at_ms: i64,
    started_at_ms: Option<i64>,
    finished_at_ms: Option<i64>,
}
impl OperationRow {
    fn decode(self) -> Result<Operation, StoreError> {
        Ok(Operation {
            id: self.id,
            application_id: ApplicationId::parse(self.application_id)
                .map_err(StoreError::corrupt)?,
            kind: OperationKind::deserialize(serde::de::value::StrDeserializer::<
                serde::de::value::Error,
            >::new(&self.kind))
            .map_err(StoreError::corrupt)?,
            state: OperationState::deserialize(serde::de::value::StrDeserializer::<
                serde::de::value::Error,
            >::new(&self.state))
            .map_err(StoreError::corrupt)?,
            error_code: self.error_code,
            error_message: self.error_message,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            started_at_ms: self.started_at_ms,
            finished_at_ms: self.finished_at_ms,
        })
    }
}

impl SqliteStore {
    /// Fetches an operation, including cancelled history.
    ///
    /// # Errors
    /// Returns a storage error or `NotFound`.
    pub async fn operation(&self, id: &str) -> Result<Operation, StoreError> {
        Self::operation_on(
            &mut *self.pool.acquire().await.map_err(StoreError::database)?,
            id,
        )
        .await
    }

    async fn operation_on(
        connection: &mut SqliteConnection,
        id: &str,
    ) -> Result<Operation, StoreError> {
        sqlx::query_as!(OperationRow,
            r#"SELECT id AS "id!",application_id,kind,state,error_code,error_message,created_at_ms,updated_at_ms,started_at_ms,finished_at_ms FROM operations WHERE id=?1"#, id)
            .fetch_optional(connection).await.map_err(StoreError::database)?
            .ok_or(StoreError::NotFound)?.decode()
    }

    /// Fetches the newest operation for an application.
    ///
    /// # Errors
    /// Returns a storage or decoding error.
    pub async fn latest_operation_for_application(
        &self,
        id: &ApplicationId,
    ) -> Result<Option<Operation>, StoreError> {
        let id = id.as_str();
        sqlx::query_as!(OperationRow,
            r#"SELECT id AS "id!",application_id,kind,state,error_code,error_message,created_at_ms,updated_at_ms,started_at_ms,finished_at_ms FROM operations WHERE application_id=?1 ORDER BY created_at_ms DESC,id DESC LIMIT 1"#, id)
            .fetch_optional(&self.pool).await.map_err(StoreError::database)?
            .map(OperationRow::decode).transpose()
    }

    /// Records a transition only if this is still the latest operation.
    ///
    /// # Errors
    /// Returns a storage error or `IllegalTransition` when superseded.
    pub async fn transition_operation(
        &self,
        id: &str,
        from: OperationState,
        to: OperationState,
        error: Option<(&str, &str)>,
    ) -> Result<(), StoreError> {
        if !from.can_transition_to(to) || error.is_some() != (to == OperationState::Failed) {
            return Err(StoreError::IllegalTransition);
        }
        let now = now_ms();
        let finished = to.terminal().then_some(now);
        let from = from.as_str();
        let to = to.as_str();
        let code = error.map(|e| e.0);
        let message = error.map(|e| e.1);
        let changed = sqlx::query!(
            "UPDATE operations SET state=?1,error_code=?2,error_message=?3,updated_at_ms=?4,started_at_ms=CASE WHEN ?1='requested' THEN NULL WHEN ?1='running' THEN COALESCE(started_at_ms,?4) ELSE started_at_ms END,finished_at_ms=?5 WHERE id=?6 AND state=?7 AND id=(SELECT latest.id FROM operations latest WHERE latest.application_id=operations.application_id ORDER BY latest.created_at_ms DESC,latest.id DESC LIMIT 1)",
            to,code,message,now,finished,id,from)
            .execute(&self.pool).await.map_err(StoreError::database)?.rows_affected();
        if changed == 1 {
            Ok(())
        } else {
            Err(StoreError::IllegalTransition)
        }
    }

    /// Stores the latest error while a deletion remains running.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn record_operation_error(
        &self,
        operation: &Operation,
        code: &str,
        message: &str,
    ) -> Result<(), StoreError> {
        let now = now_ms();
        let mut tx = self.begin_immediate().await?;
        sqlx::query!("UPDATE operations SET error_code=?1,error_message=?2,updated_at_ms=?3 WHERE id=?4 AND state='running'",code,message,now,operation.id)
            .execute(&mut *tx).await.map_err(StoreError::database)?;
        sqlx::query!("UPDATE application_status SET state='deleting',message=?1,updated_at_ms=?2 WHERE application_id=(SELECT application_id FROM operations WHERE id=?3 AND kind='delete' AND state='running')",message,now,operation.id)
            .execute(&mut *tx).await.map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }

    /// Atomically completes deletion after the controller verifies resource absence.
    ///
    /// # Errors
    /// Returns a storage error or `IllegalTransition` for stale work.
    pub async fn finish_delete_operation(&self, operation: &Operation) -> Result<(), StoreError> {
        let mut tx = self.begin_immediate().await?;
        let now = now_ms();
        let changed = sqlx::query!("UPDATE operations SET state='succeeded',error_code=NULL,error_message=NULL,updated_at_ms=?1,finished_at_ms=?1 WHERE id=?2 AND kind='delete' AND state='running'",now,operation.id)
            .execute(&mut *tx).await.map_err(StoreError::database)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::IllegalTransition);
        }
        let app_id = operation.application_id.as_str();
        sqlx::query!("UPDATE applications SET deleted_at_ms=?1,updated_at_ms=?1 WHERE id=?2 AND delete_intent=1",now,app_id)
            .execute(&mut *tx).await.map_err(StoreError::database)?;
        sqlx::query!(
            "DELETE FROM application_status WHERE application_id=?1",
            app_id
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }

    /// Requests another attempt of the current failed or cancelled operation.
    ///
    /// # Errors
    /// Returns a storage error or `IllegalTransition` if superseded.
    pub async fn retry_operation(&self, operation: &Operation) -> Result<Operation, StoreError> {
        let mut tx = self.begin_immediate().await?;
        let now = now_ms();
        let changed = sqlx::query!("UPDATE operations SET state='requested',error_code=NULL,error_message=NULL,started_at_ms=NULL,finished_at_ms=NULL,updated_at_ms=?1 WHERE id=?2 AND state IN ('failed','cancelled') AND id=(SELECT latest.id FROM operations latest WHERE latest.application_id=operations.application_id ORDER BY latest.created_at_ms DESC,latest.id DESC LIMIT 1)",now,operation.id)
            .execute(&mut *tx).await.map_err(StoreError::database)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::IllegalTransition);
        }
        let state = if operation.kind == OperationKind::Delete {
            "deleting"
        } else {
            "pending"
        };
        let app_id = operation.application_id.as_str();
        Self::write_status(&mut tx, app_id, state, None, now).await?;
        let operation = Self::operation_on(&mut tx, &operation.id).await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(operation)
    }

    /// Returns interrupted operations to the requested state on process startup.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn recover_interrupted(&self) -> Result<u64, StoreError> {
        let now = now_ms();
        Ok(sqlx::query!("UPDATE operations SET state='requested',updated_at_ms=?1,started_at_ms=NULL,error_code=NULL,error_message=NULL WHERE state='running'",now)
            .execute(&self.pool).await.map_err(StoreError::database)?.rows_affected())
    }

    /// Prunes old terminal history, always retaining the latest operation per app.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn prune_finished_operations(&self, cutoff_ms: i64) -> Result<u64, StoreError> {
        Ok(sqlx::query!("DELETE FROM operations WHERE finished_at_ms < ?1 AND id != (SELECT latest.id FROM operations latest WHERE latest.application_id=operations.application_id ORDER BY latest.created_at_ms DESC,latest.id DESC LIMIT 1)",cutoff_ms)
            .execute(&self.pool).await.map_err(StoreError::database)?.rows_affected())
    }

    pub(super) async fn insert_operation(
        tx: &mut Transaction<'_, Sqlite>,
        app: &ApplicationId,
        kind: OperationKind,
        now: i64,
    ) -> Result<Operation, StoreError> {
        let app_id = app.as_str();
        sqlx::query!("UPDATE operations SET state='cancelled',error_code=NULL,error_message=NULL,finished_at_ms=?1,updated_at_ms=?1 WHERE application_id=?2 AND state IN ('requested','running')",now,app_id)
            .execute(&mut **tx).await.map_err(StoreError::database)?;
        let id = new_id("operation");
        let kind = kind.as_str();
        sqlx::query!("INSERT INTO operations(id,application_id,kind,state,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,'requested',?4,?4)",id,app_id,kind,now)
            .execute(&mut **tx).await.map_err(StoreError::database)?;
        Self::operation_on(tx, &id).await
    }
}
