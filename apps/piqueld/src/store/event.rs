//! Append-only diagnostic history, independent of operation retention.
use super::{ApplicationId, SqliteStore, StoreError, page_limit};
use piqueld_core::{Event, api::Page};
use sqlx::{Sqlite, Transaction};

impl SqliteStore {
    pub(super) async fn operation_event(
        tx: &mut Transaction<'_, Sqlite>,
        id: &str,
        kind: &str,
        message: Option<&str>,
        now: i64,
    ) -> Result<(), StoreError> {
        sqlx::query!("INSERT INTO events(application_id,operation_id,generation,attempt,kind,message,error_code,phase,resource,created_at_ms) SELECT application_id,id,generation,attempt,?1,?2,error_code,phase,resource,?3 FROM operations WHERE id=?4",kind,message,now,id)
            .execute(&mut **tx).await.map_err(StoreError::database)?;
        Ok(())
    }

    pub(crate) async fn maintenance_event(
        &self,
        id: &str,
        phase: &str,
        resource: &str,
        error: Option<(&str, &str)>,
    ) -> Result<(), StoreError> {
        let _writer = self.writers.lock().await;
        let now = super::now_ms();
        let kind = if error.is_some() {
            "active_target_repair_failed"
        } else {
            "active_target_repaired"
        };
        let code = error.map(|error| error.0);
        let message = error.map(|error| error.1);
        sqlx::query!("INSERT INTO events(application_id,operation_id,generation,kind,phase,resource,error_code,message,created_at_ms) SELECT operations.application_id,operations.id,applications.resolved_generation,?1,?2,?3,?4,?5,?6 FROM operations JOIN applications ON applications.id=operations.application_id WHERE operations.id=?7",kind,phase,resource,code,message,now,id).execute(&self.pool).await.map_err(StoreError::database)?;
        Ok(())
    }

    /// Reads events oldest first, optionally filtered by application, including deleted applications.
    /// # Errors
    /// Returns storage errors or invalid pagination errors.
    pub async fn events(
        &self,
        application: Option<&ApplicationId>,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<Event>, StoreError> {
        let fetch_limit = page_limit(limit)? + 1;
        let after = cursor
            .map(|value| {
                value
                    .strip_prefix("v1:")
                    .ok_or(StoreError::InvalidInput)
                    .and_then(|id| id.parse::<i64>().map_err(StoreError::invalid_input))
            })
            .transpose()?
            .unwrap_or(0);
        if after < 0 {
            return Err(StoreError::InvalidInput);
        }
        let application = application.map(ApplicationId::as_str);
        let mut rows=sqlx::query!("SELECT id,application_id,operation_id,generation,attempt,kind,message,error_code,phase,resource,created_at_ms FROM events WHERE id>?1 AND (?2 IS NULL OR application_id=?2) ORDER BY id LIMIT ?3",after,application,fetch_limit).fetch_all(&self.pool).await.map_err(StoreError::database)?;
        let more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = more
            .then(|| rows.last().map(|row| format!("v1:{}", row.id)))
            .flatten();
        let items = rows
            .into_iter()
            .map(|row| {
                Ok(Event {
                    id: row.id,
                    application_id: row
                        .application_id
                        .map(ApplicationId::parse)
                        .transpose()
                        .map_err(StoreError::corrupt)?,
                    operation_id: row.operation_id,
                    generation: row
                        .generation
                        .map(u64::try_from)
                        .transpose()
                        .map_err(StoreError::corrupt)?,
                    attempt: row
                        .attempt
                        .map(u64::try_from)
                        .transpose()
                        .map_err(StoreError::corrupt)?,
                    kind: row.kind,
                    message: row.message,
                    error_code: row.error_code,
                    phase: row.phase,
                    resource: row.resource,
                    created_at_ms: row.created_at_ms,
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        Ok(Page { items, next_cursor })
    }

    /// Prunes old events without changing application or operation state.
    /// # Errors
    /// Returns a storage error.
    pub async fn prune_events(&self, cutoff_ms: i64) -> Result<u64, StoreError> {
        let _writer = self.writers.lock().await;
        Ok(
            sqlx::query!("DELETE FROM events WHERE created_at_ms<?1", cutoff_ms)
                .execute(&self.pool)
                .await
                .map_err(StoreError::database)?
                .rows_affected(),
        )
    }
}
