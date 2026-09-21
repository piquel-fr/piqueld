//! Persist intent before a runtime request and record its observed outcome afterward.
use super::{Store, StoreError, new_id, now_ms};
use piqueld_core::observability::{Diagnostic, EventScope};
use sqlx::{Sqlite, Transaction};

#[derive(Clone, Debug)]
pub(crate) struct JournalAction {
    pub(crate) id: String,
    operation_id: Option<String>,
    application_id: Option<String>,
    generation: Option<i64>,
    phase: String,
    resource: Option<String>,
    attempt: Option<i64>,
    started_at_ms: i64,
}
impl Store {
    pub(crate) async fn begin_action(
        &self,
        operation: Option<&str>,
        phase: &str,
        resource: Option<&str>,
    ) -> Result<JournalAction, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let op = match operation {
            Some(id) => Some(Self::operation_on(&mut tx, id).await?),
            None => None,
        };
        let action = JournalAction {
            id: new_id("action"),
            operation_id: operation.map(str::to_owned),
            application_id: op.as_ref().map(|o| o.application_id.to_string()),
            generation: op
                .as_ref()
                .map(|o| i64::try_from(o.generation))
                .transpose()
                .map_err(StoreError::corrupt)?,
            phase: phase.to_owned(),
            resource: resource.map(str::to_owned),
            attempt: op
                .as_ref()
                .map(|o| i64::try_from(o.attempt))
                .transpose()
                .map_err(StoreError::corrupt)?,
            started_at_ms: now_ms(),
        };
        sqlx::query!("INSERT INTO active_actions(id,operation_id,application_id,generation,phase,resource,attempt,started_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",action.id,action.operation_id,action.application_id,action.generation,action.phase,action.resource,action.attempt,action.started_at_ms).execute(&mut *tx).await.map_err(StoreError::database)?;
        Self::action_event_on(&mut tx, &action, "action_started", None, None, None).await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(action)
    }
    pub(crate) async fn action_request(
        &self,
        action: &JournalAction,
        retry: u32,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let retry = i64::from(retry);
        let changed = sqlx::query!(
            "UPDATE active_actions SET retry=?1 WHERE id=?2",
            retry,
            action.id
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        if changed != 1 {
            return Err(StoreError::IllegalTransition);
        }
        Self::action_event_on(&mut tx, action, "action_requested", Some(retry), None, None).await?;
        tx.commit().await.map_err(StoreError::database)
    }
    pub(crate) async fn action_retry(
        &self,
        action: &JournalAction,
        retry: u32,
        delay: std::time::Duration,
        diagnostic: &Diagnostic,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let message = format!(
            "{}; retry scheduled in {} ms",
            diagnostic.summary,
            delay.as_millis()
        );
        Self::action_event_on(
            &mut tx,
            action,
            "action_retry",
            Some(i64::from(retry)),
            Some(diagnostic),
            Some(&message),
        )
        .await?;
        let delay_ms = i64::try_from(delay.as_millis()).unwrap_or(i64::MAX);
        sqlx::query!(
            "UPDATE events SET retry_delay_ms=?1 WHERE id=last_insert_rowid()",
            delay_ms
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }
    pub(crate) async fn finish_action(
        &self,
        action: &JournalAction,
        diagnostic: Option<Diagnostic>,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let active =
            sqlx::query_scalar!("SELECT COUNT(*) FROM active_actions WHERE id=?1", action.id)
                .fetch_one(&mut *tx)
                .await
                .map_err(StoreError::database)?;
        if active == 0 {
            return Err(StoreError::IllegalTransition);
        }
        if let Some(diagnostic) = &diagnostic {
            tracing::error!(diagnostic_id=%diagnostic.id,action_id=%action.id,operation_id=?action.operation_id,code=%diagnostic.code,"action failed");
        }
        Self::action_event_on(
            &mut tx,
            action,
            if diagnostic.is_some() {
                "action_failed"
            } else {
                "action_succeeded"
            },
            None,
            diagnostic.as_ref(),
            None,
        )
        .await?;
        sqlx::query!("DELETE FROM active_actions WHERE id=?1", action.id)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }
    async fn action_event_on(
        tx: &mut Transaction<'_, Sqlite>,
        action: &JournalAction,
        kind: &str,
        retry: Option<i64>,
        diagnostic: Option<&Diagnostic>,
        message: Option<&str>,
    ) -> Result<(), StoreError> {
        let diagnostic = diagnostic.cloned().map(|mut d| {
            if action.application_id.is_none() {
                d.scope = EventScope::Daemon;
            }
            d
        });
        let diagnostic = diagnostic.as_ref();
        let now = now_ms();
        let duration = matches!(kind, "action_succeeded" | "action_failed")
            .then_some(now.saturating_sub(action.started_at_ms));
        let scope = diagnostic
            .map_or(
                if action.application_id.is_some() {
                    EventScope::Application
                } else {
                    EventScope::Daemon
                },
                |d| d.scope,
            )
            .as_str();
        let code = diagnostic.map(|d| d.code.as_str());
        let id = diagnostic.map(|d| d.id.as_str());
        let message = message.or_else(|| diagnostic.map(|d| d.summary.as_str()));
        let json = diagnostic
            .map(serde_json::to_string)
            .transpose()
            .map_err(StoreError::corrupt)?;
        sqlx::query!("INSERT INTO events(application_id,operation_id,generation,attempt,kind,message,error_code,phase,resource,created_at_ms,scope,action_id,retry,duration_ms,diagnostic_id,diagnostic_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",action.application_id,action.operation_id,action.generation,action.attempt,kind,message,code,action.phase,action.resource,now,scope,action.id,retry,duration,id,json).execute(&mut **tx).await.map_err(StoreError::database)?;
        Ok(())
    }
    /// Closes incomplete action records without inventing an external outcome.
    /// # Errors
    /// Returns a storage error; reconciliation must not resume if it cannot record recovery.
    pub async fn interrupt_actions(&self, operation: Option<&str>) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let now = now_ms();
        sqlx::query!("INSERT INTO events(application_id,operation_id,generation,attempt,kind,message,phase,resource,created_at_ms,scope,action_id,retry) SELECT application_id,operation_id,generation,attempt,'action_outcome_unknown','Execution was interrupted before its result was committed; reconciliation will inspect current runtime state',phase,resource,?1,CASE WHEN application_id IS NULL THEN 'daemon' ELSE 'application' END,id,retry FROM active_actions WHERE ?2 IS NULL OR operation_id=?2",now,operation).execute(&mut *tx).await.map_err(StoreError::database)?;
        sqlx::query!(
            "DELETE FROM active_actions WHERE ?1 IS NULL OR operation_id=?1",
            operation
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }
}
