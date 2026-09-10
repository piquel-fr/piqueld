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
    generation: i64,
    attempt: i64,
    consecutive_failures: i64,
    phase: Option<String>,
    resource: Option<String>,
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
            generation: u64::try_from(self.generation).map_err(StoreError::corrupt)?,
            attempt: u64::try_from(self.attempt).map_err(StoreError::corrupt)?,
            consecutive_failures: u64::try_from(self.consecutive_failures)
                .map_err(StoreError::corrupt)?,
            phase: self.phase,
            resource: self.resource,
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

    pub(crate) async fn operation_on(
        connection: &mut SqliteConnection,
        id: &str,
    ) -> Result<Operation, StoreError> {
        sqlx::query_as!(OperationRow,
            r#"SELECT id AS "id!",application_id,kind,state,generation,attempt,consecutive_failures,phase,resource,error_code,error_message,created_at_ms,updated_at_ms,started_at_ms,finished_at_ms FROM operations WHERE id=?1"#, id)
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
            r#"SELECT id AS "id!",application_id,kind,state,generation,attempt,consecutive_failures,phase,resource,error_code,error_message,created_at_ms,updated_at_ms,started_at_ms,finished_at_ms FROM operations WHERE application_id=?1 ORDER BY created_at_ms DESC,id DESC LIMIT 1"#, id)
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
        let terminal = to.terminal();
        let (_writer, mut tx) = self.begin_immediate().await?;
        let finished = terminal.then_some(now);
        let from = from.as_str();
        let to = to.as_str();
        let code = error.map(|e| e.0);
        let message = error.map(|e| e.1);
        let changed = sqlx::query!(
            "UPDATE operations SET consecutive_failures=CASE WHEN ?1='failed' THEN consecutive_failures+1 WHEN ?1='succeeded' THEN 0 ELSE consecutive_failures END,attempt=attempt+CASE WHEN ?1='running' AND state!='running' THEN 1 ELSE 0 END,state=?1,error_code=?2,error_message=?3,updated_at_ms=?4,started_at_ms=CASE WHEN ?1='requested' THEN NULL WHEN ?1='running' THEN COALESCE(started_at_ms,?4) ELSE started_at_ms END,finished_at_ms=?5 WHERE id=?6 AND state=?7 AND id=(SELECT latest.id FROM operations latest WHERE latest.application_id=operations.application_id ORDER BY latest.created_at_ms DESC,latest.id DESC LIMIT 1)",
            to,code,message,now,finished,id,from)
            .execute(&mut *tx).await.map_err(StoreError::database)?.rows_affected();
        if changed == 1 {
            if from != to {
                let kind = match to {
                    "running" => "operation_started",
                    "succeeded" => "operation_succeeded",
                    "failed" => "operation_failed",
                    "cancelled" => "operation_cancelled",
                    _ => "reconciliation_requested",
                };
                Self::operation_event(&mut tx, id, kind, message, now).await?;
            }
            tx.commit().await.map_err(StoreError::database)?;
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
        let (_writer, mut tx) = self.begin_immediate().await?;
        let changed=sqlx::query!("UPDATE operations SET consecutive_failures=consecutive_failures+1,error_code=?1,error_message=?2,updated_at_ms=?3 WHERE id=?4 AND state='running'",code,message,now,operation.id)
            .execute(&mut *tx).await.map_err(StoreError::database)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::IllegalTransition);
        }
        sqlx::query!("UPDATE application_status SET state='deleting',message=?1,updated_at_ms=?2 WHERE application_id=(SELECT application_id FROM operations WHERE id=?3 AND kind='delete' AND state='running')",message,now,operation.id)
            .execute(&mut *tx).await.map_err(StoreError::database)?;
        Self::operation_event(
            &mut tx,
            &operation.id,
            "operation_failed",
            Some(message),
            now,
        )
        .await?;
        tx.commit().await.map_err(StoreError::database)
    }

    /// Atomically completes deletion after the controller verifies resource absence.
    ///
    /// # Errors
    /// Returns a storage error or `IllegalTransition` for stale work.
    pub async fn finish_delete_operation(&self, operation: &Operation) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let now = now_ms();
        let changed = sqlx::query!("UPDATE operations SET state='succeeded',consecutive_failures=0,error_code=NULL,error_message=NULL,updated_at_ms=?1,finished_at_ms=?1 WHERE id=?2 AND kind='delete' AND state='running'",now,operation.id)
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
        Self::operation_event(&mut tx, &operation.id, "deletion_completed", None, now).await?;
        tx.commit().await.map_err(StoreError::database)
    }

    /// Requests another attempt of the latest terminal operation or failed deletion.
    ///
    /// # Errors
    /// Returns a storage error or `IllegalTransition` if superseded.
    pub async fn retry_operation(&self, operation: &Operation) -> Result<Operation, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let result = Self::retry_operation_on(&mut tx, operation).await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(result)
    }

    pub(crate) async fn retry_operation_on(
        tx: &mut Transaction<'_, Sqlite>,
        operation: &Operation,
    ) -> Result<Operation, StoreError> {
        let now = now_ms();
        let changed = sqlx::query!("UPDATE operations SET generation=(SELECT generation FROM applications WHERE id=operations.application_id),phase=NULL,resource=NULL,state='requested',error_code=NULL,error_message=NULL,started_at_ms=NULL,finished_at_ms=NULL,updated_at_ms=?1 WHERE id=?2 AND (state IN ('succeeded','failed','cancelled') OR (state='running' AND error_code IS NOT NULL)) AND id=(SELECT latest.id FROM operations latest WHERE latest.application_id=operations.application_id ORDER BY latest.created_at_ms DESC,latest.id DESC LIMIT 1)",now,operation.id)
            .execute(&mut **tx).await.map_err(StoreError::database)?.rows_affected();
        if changed != 1 {
            let current = Self::operation_on(tx, &operation.id).await?;
            // Another caller may already have requested or started this retry.
            // Superseded operations remain cancelled and cannot be resurrected.
            if matches!(
                current.state,
                OperationState::Requested | OperationState::Running
            ) {
                return Ok(current);
            }
            return Err(StoreError::IllegalTransition);
        }
        let state = if operation.kind == OperationKind::Delete {
            "deleting"
        } else {
            "pending"
        };
        let app_id = operation.application_id.as_str();
        Self::write_status(tx, app_id, state, None, now).await?;
        Self::operation_event(tx, &operation.id, "reconciliation_requested", None, now).await?;
        let operation = Self::operation_on(tx, &operation.id).await?;
        Ok(operation)
    }

    /// Updates the currently executing phase/resource without emitting polling events.
    /// # Errors
    /// Returns storage errors or a stale-operation conflict.
    pub async fn progress(
        &self,
        id: &str,
        phase: &str,
        resource: Option<&str>,
    ) -> Result<(), StoreError> {
        let _writer = self.writers.lock().await;
        let changed=sqlx::query!("UPDATE operations SET phase=?1,resource=?2 WHERE id=?3 AND state='running' AND id=(SELECT latest.id FROM operations latest WHERE latest.application_id=operations.application_id ORDER BY latest.created_at_ms DESC,latest.id DESC LIMIT 1)",phase,resource,id).execute(&self.pool).await.map_err(StoreError::database)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::IllegalTransition);
        }
        Ok(())
    }

    pub(crate) async fn mutation_event(&self, id: &str) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        Self::operation_event(&mut tx, id, "resource_mutated", None, now_ms()).await?;
        tx.commit().await.map_err(StoreError::database)
    }

    pub(crate) async fn is_promoted(&self, id: &str) -> Result<bool, StoreError> {
        Ok(
            sqlx::query_scalar!("SELECT promoted FROM operations WHERE id=?1", id)
                .fetch_one(&self.pool)
                .await
                .map_err(StoreError::database)?
                != 0,
        )
    }

    /// Returns interrupted operations to the requested state on process startup.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn recover_interrupted(&self) -> Result<u64, StoreError> {
        let now = now_ms();
        let (_writer, mut tx) = self.begin_immediate().await?;
        sqlx::query!("INSERT INTO events(application_id,operation_id,generation,attempt,kind,message,error_code,phase,resource,created_at_ms) SELECT application_id,id,generation,attempt,'operation_interrupted','daemon restarted during execution',error_code,phase,resource,?1 FROM operations WHERE state='running'",now).execute(&mut *tx).await.map_err(StoreError::database)?;
        let count=sqlx::query!("UPDATE operations SET state='requested',updated_at_ms=?1,started_at_ms=NULL,error_code=NULL,error_message=NULL WHERE state='running'",now)
            .execute(&mut *tx).await.map_err(StoreError::database)?.rows_affected();
        tx.commit().await.map_err(StoreError::database)?;
        Ok(count)
    }

    /// Prunes old terminal history, always retaining the latest operation per app.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn prune_finished_operations(&self, cutoff_ms: i64) -> Result<u64, StoreError> {
        let _writer = self.writers.lock().await;
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
        sqlx::query!("INSERT INTO events(application_id,operation_id,generation,attempt,kind,message,error_code,phase,resource,created_at_ms) SELECT application_id,id,generation,attempt,'operation_cancelled','superseded by newer intent',error_code,phase,resource,?1 FROM operations WHERE application_id=?2 AND state IN ('requested','running')",now,app_id).execute(&mut **tx).await.map_err(StoreError::database)?;
        sqlx::query!("UPDATE operations SET state='cancelled',error_code=NULL,error_message=NULL,finished_at_ms=?1,updated_at_ms=?1 WHERE application_id=?2 AND state IN ('requested','running')",now,app_id)
            .execute(&mut **tx).await.map_err(StoreError::database)?;
        let id = new_id("operation");
        let kind = kind.as_str();
        sqlx::query!("INSERT INTO operations(id,application_id,generation,kind,state,created_at_ms,updated_at_ms) SELECT ?1,?2,generation,?3,'requested',?4,?4 FROM applications WHERE id=?2",id,app_id,kind,now)
            .execute(&mut **tx).await.map_err(StoreError::database)?;
        let event = match kind {
            "apply" => "application_applied",
            "refresh" => "refresh_requested",
            _ => "deletion_requested",
        };
        Self::operation_event(tx, &id, event, None, now).await?;
        Self::operation_on(tx, &id).await
    }
}
