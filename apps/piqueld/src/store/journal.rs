//! Persist intent before a runtime request and record its observed outcome afterward.
use super::{Attribution, EnvironmentId, Store, StoreError, new_id, now_ms};
use piqueld_core::observability::{Diagnostic, DiagnosticCode, EventScope};
use sqlx::{Sqlite, Transaction};

/// Handle to one journaled runtime request, recorded in `active_actions` until
/// finished. Lifecycle: `begin_action` (or `begin_application_action`), then
/// `action_request` before each attempt, `action_retry` after a retryable
/// failure, and finally `finish_action`. Actions left open by a crash are closed
/// by `interrupt_actions` as `action_outcome_unknown`.
#[derive(Clone, Debug)]
pub(crate) struct JournalAction {
    pub(crate) id: String,
    operation_id: Option<String>,
    environment_id: Option<String>,
    generation: Option<i64>,
    phase: String,
    resource: Option<String>,
    attempt: Option<i64>,
    started_at_ms: i64,
    /// Who requested the action: its operation's actor when it started (a
    /// later restart by someone else does not change it), or the caller of a
    /// request outside any operation.
    actor_user_id: Option<String>,
    actor_credential_id: Option<String>,
    actor_operator_uid: Option<i64>,
}
impl JournalAction {
    /// A daemon-scoped action outside any operation, starting now.
    fn daemon(phase: &str, resource: Option<&str>) -> Self {
        Self {
            id: new_id("action"),
            operation_id: None,
            environment_id: None,
            generation: None,
            phase: phase.to_owned(),
            resource: resource.map(str::to_owned),
            attempt: None,
            started_at_ms: now_ms(),
            actor_user_id: None,
            actor_credential_id: None,
            actor_operator_uid: None,
        }
    }
}
impl Store {
    /// Journals a runtime request before it is made, optionally under an
    /// operation whose environment, generation, attempt, and actor are copied
    /// onto the action. Without an operation the action is daemon-scoped.
    pub(crate) async fn begin_action(
        &self,
        operation: Option<&str>,
        phase: &str,
        resource: Option<&str>,
    ) -> Result<JournalAction, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let mut action = JournalAction {
            operation_id: operation.map(str::to_owned),
            ..JournalAction::daemon(phase, resource)
        };
        if let Some(id) = operation {
            let op = Self::operation_on(&mut tx, id).await?;
            action.environment_id = Some(op.environment_id.to_string());
            action.generation = Some(i64::try_from(op.generation).map_err(StoreError::corrupt)?);
            action.attempt = Some(i64::try_from(op.attempt).map_err(StoreError::corrupt)?);
        }
        Self::attribute_action_on(&mut tx, &mut action, operation).await?;
        Self::start_action_on(&mut tx, &action).await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(action)
    }
    /// Journals a daemon-scoped runtime request made on behalf of
    /// `requested_by`'s operation, e.g. shared gateway changes a deployment
    /// needs: it records the operation's actor but belongs to no environment.
    pub(crate) async fn begin_daemon_action(
        &self,
        requested_by: Option<&str>,
        phase: &str,
        resource: Option<&str>,
    ) -> Result<JournalAction, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let mut action = JournalAction::daemon(phase, resource);
        Self::attribute_action_on(&mut tx, &mut action, requested_by).await?;
        Self::start_action_on(&mut tx, &action).await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(action)
    }
    /// Journals an environment-owned runtime request made outside any
    /// operation on behalf of `actor`.
    pub(crate) async fn begin_application_action(
        &self,
        actor: Attribution<'_>,
        application: &EnvironmentId,
        phase: &str,
        resource: Option<&str>,
    ) -> Result<JournalAction, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let action = JournalAction {
            environment_id: Some(application.to_string()),
            actor_user_id: actor.user_id.map(str::to_owned),
            actor_credential_id: actor.credential_id.map(str::to_owned),
            actor_operator_uid: actor.operator_uid().map(i64::from),
            ..JournalAction::daemon(phase, resource)
        };
        Self::start_action_on(&mut tx, &action).await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(action)
    }
    /// Snapshots `operation`'s current actor onto `action`, so restarting the
    /// operation later does not reattribute it.
    async fn attribute_action_on(
        tx: &mut Transaction<'_, Sqlite>,
        action: &mut JournalAction,
        operation: Option<&str>,
    ) -> Result<(), StoreError> {
        let actor = sqlx::query!(
            "SELECT actor_user_id,actor_credential_id,actor_operator_uid FROM operations WHERE id=?1",
            operation,
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        if let Some(actor) = actor {
            action.actor_user_id = actor.actor_user_id;
            action.actor_credential_id = actor.actor_credential_id;
            action.actor_operator_uid = actor.actor_operator_uid;
        }
        Ok(())
    }
    /// Inserts the `active_actions` row and its `action_started` event.
    async fn start_action_on(
        tx: &mut Transaction<'_, Sqlite>,
        action: &JournalAction,
    ) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO active_actions(id,operation_id,environment_id,generation,phase,resource,attempt,started_at_ms,
            actor_user_id,actor_credential_id,actor_operator_uid)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            action.id,
            action.operation_id,
            action.environment_id,
            action.generation,
            action.phase,
            action.resource,
            action.attempt,
            action.started_at_ms,
            action.actor_user_id,
            action.actor_credential_id,
            action.actor_operator_uid,
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Self::action_event_on(tx, action, "action_started", None, None, None).await
    }
    /// Records that attempt `retry` is about to be sent to the runtime.
    /// Fails with `StoreError::IllegalTransition` if the action is no longer active.
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
            action.id,
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
    /// Records a retryable failure and the backoff before the next attempt.
    /// The delay is written onto the event just inserted via `last_insert_rowid()`.
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
            delay_ms,
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }
    /// Records the action's final outcome (`action_succeeded`, or
    /// `action_failed` with a diagnostic) and removes it from `active_actions`.
    /// Cancelled or superseded failures are logged at info rather than error.
    /// Fails with `StoreError::IllegalTransition` if the action was already closed.
    pub(crate) async fn finish_action(
        &self,
        action: &JournalAction,
        diagnostic: Option<Diagnostic>,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let active =
            sqlx::query_scalar!("SELECT COUNT(*) FROM active_actions WHERE id=?1", action.id,)
                .fetch_one(&mut *tx)
                .await
                .map_err(StoreError::database)?;
        if active == 0 {
            return Err(StoreError::IllegalTransition);
        }
        match &diagnostic {
            Some(diagnostic)
                if matches!(
                    DiagnosticCode::parse(&diagnostic.code),
                    Some(DiagnosticCode::Cancelled | DiagnosticCode::Superseded)
                ) =>
            {
                tracing::info!(diagnostic_id=%diagnostic.id,action_id=%action.id,operation_id=?action.operation_id,code=%diagnostic.code,"action stopped");
            }
            Some(diagnostic) => {
                tracing::error!(diagnostic_id=%diagnostic.id,action_id=%action.id,operation_id=?action.operation_id,code=%diagnostic.code,"action failed");
            }
            None => {}
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
        sqlx::query!("DELETE FROM active_actions WHERE id=?1", action.id,)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }
    /// Appends an action lifecycle event to `events`. Daemon-scoped actions
    /// force their diagnostic scope to `Daemon`; terminal kinds also record the
    /// action's duration. `message` overrides the diagnostic summary.
    async fn action_event_on(
        tx: &mut Transaction<'_, Sqlite>,
        action: &JournalAction,
        kind: &str,
        retry: Option<i64>,
        diagnostic: Option<&Diagnostic>,
        message: Option<&str>,
    ) -> Result<(), StoreError> {
        let diagnostic = diagnostic.cloned().map(|mut d| {
            if action.environment_id.is_none() {
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
                if action.environment_id.is_some() {
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
        sqlx::query!(
            "INSERT INTO events(application_id,environment_id,operation_id,generation,attempt,kind,message,
            error_code,phase,resource,created_at_ms,scope,action_id,retry,duration_ms,diagnostic_id,
            diagnostic_json,actor_user_id,actor_credential_id,actor_operator_uid)
            VALUES((SELECT application_id FROM environments WHERE id=?1),?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
            action.environment_id,
            action.operation_id,
            action.generation,
            action.attempt,
            kind,
            message,
            code,
            action.phase,
            action.resource,
            now,
            scope,
            action.id,
            retry,
            duration,
            id,
            json,
            action.actor_user_id,
            action.actor_credential_id,
            action.actor_operator_uid,
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }
    /// Closes incomplete action records without inventing an external outcome.
    /// Each open action (all of them, or only `operation`'s) gets an
    /// `action_outcome_unknown` event and is removed from `active_actions`.
    ///
    /// # Errors
    /// Returns a storage error; reconciliation must not resume if it cannot record recovery.
    pub async fn interrupt_actions(&self, operation: Option<&str>) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let now = now_ms();
        sqlx::query!(
            "INSERT INTO events(application_id,environment_id,operation_id,generation,attempt,kind,message,phase,
            resource,created_at_ms,scope,action_id,retry,actor_user_id,actor_credential_id,actor_operator_uid)
            SELECT (SELECT application_id FROM environments WHERE id=active_actions.environment_id),
            environment_id,operation_id,generation,attempt,'action_outcome_unknown',
            'Execution was interrupted before its result was committed; reconciliation will inspect current runtime state',
            phase,resource,?1,CASE WHEN environment_id IS NULL THEN 'daemon' ELSE 'application' END,
            id,retry,actor_user_id,actor_credential_id,actor_operator_uid
            FROM active_actions
            WHERE ?2 IS NULL OR operation_id=?2",
            now,
            operation,
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        sqlx::query!(
            "DELETE FROM active_actions WHERE ?1 IS NULL OR operation_id=?1",
            operation,
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }
}
