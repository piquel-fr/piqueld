//! Immutable diagnostic history, independent of operation retention.
use super::{ApplicationId, Store, StoreError, new_id, now_ms, page_limit};
use piqueld_core::{
    Event,
    api::Page,
    observability::{Diagnostic, EventFilter, EventScope},
};
use sqlx::{Sqlite, Transaction};

impl Store {
    pub(super) async fn operation_event(
        tx: &mut Transaction<'_, Sqlite>,
        id: &str,
        kind: &str,
        message: Option<&str>,
        now: i64,
    ) -> Result<(), StoreError> {
        let operation = Self::operation_on(tx, id).await?;
        let diagnostic = if let Some(code) = &operation.error_code {
            let attempt = i64::try_from(operation.attempt).map_err(StoreError::corrupt)?;
            let prior = sqlx::query_scalar!("SELECT diagnostic_json FROM events WHERE operation_id=?1 AND attempt=?2 AND error_code=?3 AND diagnostic_json IS NOT NULL ORDER BY id DESC LIMIT 1",id,attempt,code).fetch_optional(&mut **tx).await.map_err(StoreError::database)?.flatten();
            Some(match prior {
                Some(json) => serde_json::from_str(&json).map_err(StoreError::corrupt)?,
                None => Diagnostic::new(
                    new_id("diagnostic"),
                    code,
                    message.unwrap_or("operation failed").to_owned(),
                ),
            })
        } else {
            None
        };
        let scope = diagnostic
            .as_ref()
            .map_or(EventScope::Application, |d| d.scope)
            .as_str();
        let diagnostic_id = diagnostic.as_ref().map(|d| d.id.as_str());
        let json = diagnostic
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(StoreError::corrupt)?;
        sqlx::query!("INSERT INTO events(application_id,operation_id,generation,attempt,kind,message,error_code,phase,resource,created_at_ms,scope,diagnostic_id,diagnostic_json) SELECT application_id,id,generation,attempt,?1,?2,error_code,phase,resource,?3,?4,?5,?6 FROM operations WHERE id=?7",kind,message,now,scope,diagnostic_id,json,id).execute(&mut **tx).await.map_err(StoreError::database)?;
        Ok(())
    }

    /// Reads legacy oldest-first event pages.
    /// # Errors
    /// Returns storage or pagination errors.
    pub async fn events(
        &self,
        application: Option<&ApplicationId>,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<Event>, StoreError> {
        self.filtered_events(
            &EventFilter {
                application_id: application.map(ToString::to_string),
                ..EventFilter::default()
            },
            cursor,
            limit,
        )
        .await
    }

    /// Reads indexed, independently understandable history in either direction.
    /// # Errors
    /// Returns storage, decoding or invalid selection errors.
    pub async fn filtered_events(
        &self,
        filter: &EventFilter,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<Event>, StoreError> {
        let fetch = page_limit(limit)? + 1;
        let cursor = cursor
            .map(Self::event_cursor)
            .transpose()?
            .unwrap_or(if filter.descending { i64::MAX } else { 0 });
        if filter
            .since_ms
            .zip(filter.until_ms)
            .is_some_and(|(a, b)| a > b)
        {
            return Err(StoreError::InvalidInput);
        }
        if let Some(id) = &filter.application_id {
            ApplicationId::parse(id).map_err(StoreError::invalid_input)?;
        }
        let attempt = filter
            .attempt
            .map(i64::try_from)
            .transpose()
            .map_err(StoreError::invalid_input)?;
        let scope = filter.scope.map(EventScope::as_str);
        let descending = filter.descending;
        let mut rows = sqlx::query!("SELECT id,application_id,operation_id,generation,attempt,kind,message,error_code,phase,resource,created_at_ms,scope,action_id,retry,retry_delay_ms,duration_ms,request_id,diagnostic_json FROM events WHERE ((?1=0 AND id>?2) OR (?1=1 AND id<?2)) AND (?3 IS NULL OR application_id=?3) AND (?4 IS NULL OR operation_id=?4) AND (?5 IS NULL OR attempt=?5) AND (?6 IS NULL OR action_id=?6) AND (?7 IS NULL OR kind=?7) AND (?8 IS NULL OR error_code=?8) AND (?9 IS NULL OR scope=?9) AND (?10 IS NULL OR created_at_ms>=?10) AND (?11 IS NULL OR created_at_ms<=?11) AND (?12=0 OR error_code IS NOT NULL) ORDER BY CASE WHEN ?1=0 THEN id END ASC, CASE WHEN ?1=1 THEN id END DESC LIMIT ?13",descending,cursor,filter.application_id,filter.operation_id,attempt,filter.action_id,filter.kind,filter.error_code,scope,filter.since_ms,filter.until_ms,filter.errors_only,fetch).fetch_all(&self.pool).await.map_err(StoreError::database)?;
        let more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = more
            .then(|| rows.last().map(|r| format!("v1:{}", r.id)))
            .flatten();
        let items = rows
            .into_iter()
            .map(|r| {
                Ok(Event {
                    id: r.id,
                    application_id: r
                        .application_id
                        .map(ApplicationId::parse)
                        .transpose()
                        .map_err(StoreError::corrupt)?,
                    operation_id: r.operation_id,
                    generation: r
                        .generation
                        .map(u64::try_from)
                        .transpose()
                        .map_err(StoreError::corrupt)?,
                    attempt: r
                        .attempt
                        .map(u64::try_from)
                        .transpose()
                        .map_err(StoreError::corrupt)?,
                    kind: r.kind,
                    message: r.message,
                    error_code: r.error_code,
                    phase: r.phase,
                    resource: r.resource,
                    created_at_ms: r.created_at_ms,
                    scope: match r.scope.as_str() {
                        "daemon" => EventScope::Daemon,
                        "application" => EventScope::Application,
                        _ => return Err(StoreError::Corrupt),
                    },
                    action_id: r.action_id,
                    retry: r
                        .retry
                        .map(u64::try_from)
                        .transpose()
                        .map_err(StoreError::corrupt)?,
                    retry_delay_ms: r
                        .retry_delay_ms
                        .map(u64::try_from)
                        .transpose()
                        .map_err(StoreError::corrupt)?,
                    duration_ms: r
                        .duration_ms
                        .map(u64::try_from)
                        .transpose()
                        .map_err(StoreError::corrupt)?,
                    request_id: r.request_id,
                    diagnostic: r
                        .diagnostic_json
                        .map(|json| serde_json::from_str(&json))
                        .transpose()
                        .map_err(StoreError::corrupt)?,
                })
            })
            .collect::<Result<_, StoreError>>()?;
        Ok(Page { items, next_cursor })
    }
    pub(crate) fn event_cursor(cursor: &str) -> Result<i64, StoreError> {
        cursor
            .strip_prefix("v1:")
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|id| *id >= 0)
            .ok_or(StoreError::InvalidInput)
    }
    /// Finds an exact event identity.
    /// # Errors
    /// Returns a storage error or not found.
    pub async fn event(&self, id: i64) -> Result<Event, StoreError> {
        if id <= 0 {
            return Err(StoreError::NotFound);
        }
        self.events(None, Some(&format!("v1:{}", id - 1)), 1)
            .await?
            .items
            .into_iter()
            .next()
            .filter(|e| e.id == id)
            .ok_or(StoreError::NotFound)
    }
    /// Resolves an occurrence ID to its original contextual event.
    /// # Errors
    /// Returns a storage error or not found.
    pub async fn diagnostic(&self, id: &str) -> Result<Event, StoreError> {
        let row = sqlx::query!(
            "SELECT id AS \"id!\" FROM events WHERE diagnostic_id=?1 ORDER BY id LIMIT 1",
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        self.event(row.id).await
    }
    /// Records an unexpected failure independently of any operation.
    /// # Errors
    /// Returns the underlying journal failure; callers must retain structured fallback logs.
    pub async fn record_diagnostic(
        &self,
        diagnostic: &Diagnostic,
        request_id: Option<&str>,
        application: Option<&ApplicationId>,
    ) -> Result<(), StoreError> {
        let _writer = self.writers.lock().await;
        let mut diagnostic = diagnostic.clone();
        if application.is_none() {
            diagnostic.scope = EventScope::Daemon;
        }
        let scope = diagnostic.scope.as_str();
        let app = application.map(ApplicationId::as_str);
        let json = serde_json::to_string(&diagnostic).map_err(StoreError::corrupt)?;
        let now = now_ms();
        sqlx::query!("INSERT INTO events(scope,application_id,kind,message,error_code,diagnostic_id,diagnostic_json,request_id,created_at_ms) SELECT ?1,?2,'diagnostic',?3,?4,?5,?6,?7,?8 WHERE ?1='daemon' OR EXISTS(SELECT 1 FROM applications WHERE id=?2)",scope,app,diagnostic.summary,diagnostic.code,diagnostic.id,json,request_id,now).execute(&self.pool).await.map_err(StoreError::database)?;
        Ok(())
    }
    pub(crate) async fn report_diagnostic(
        &self,
        diagnostic: &Diagnostic,
        application: Option<&ApplicationId>,
    ) {
        tracing::error!(diagnostic_id=%diagnostic.id, code=%diagnostic.code, summary=%diagnostic.summary, "control-plane failure");
        if let Err(error) = self.record_diagnostic(diagnostic, None, application).await {
            tracing::error!(diagnostic_id=%diagnostic.id,error=?error,"diagnostic journal unavailable; occurrence retained in daemon logs");
        }
    }

    /// Checks whether a stream can honestly resume at its previous position.
    /// # Errors
    /// Returns expired history or storage errors.
    pub async fn check_event_resume(&self, after: i64) -> Result<(), StoreError> {
        let pruned =
            sqlx::query_scalar!("SELECT pruned_through_id FROM history_coverage WHERE singleton=1")
                .fetch_one(&self.pool)
                .await
                .map_err(StoreError::database)?;
        if after < pruned {
            Err(StoreError::HistoryExpired)
        } else {
            Ok(())
        }
    }
    /// Prunes application-scoped events. Daemon history has a separate policy.
    /// # Errors
    /// Returns storage errors.
    pub async fn prune_events(&self, cutoff_ms: i64) -> Result<u64, StoreError> {
        self.prune_scope(cutoff_ms, EventScope::Application).await
    }
    pub(crate) async fn prune_daemon_events(&self) -> Result<(), StoreError> {
        if self.daemon_event_days > 0 {
            self.prune_scope(
                now_ms().saturating_sub(
                    i64::try_from(self.daemon_event_days.saturating_mul(86_400_000))
                        .unwrap_or(i64::MAX),
                ),
                EventScope::Daemon,
            )
            .await?;
        }
        Ok(())
    }
    async fn prune_scope(&self, cutoff: i64, scope: EventScope) -> Result<u64, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let scope = scope.as_str();
        let removed = sqlx::query!("DELETE FROM events WHERE scope=?1 AND created_at_ms<?2 AND NOT EXISTS(SELECT 1 FROM notification_deliveries d WHERE d.event_id=events.id AND d.state='pending') AND NOT EXISTS(SELECT 1 FROM active_actions a WHERE a.id=events.action_id) RETURNING id,created_at_ms",scope,cutoff).fetch_all(&mut *tx).await.map_err(StoreError::database)?;
        if let Some(last_id) = removed.iter().map(|r| r.id).max() {
            let last_time = removed
                .iter()
                .map(|r| r.created_at_ms)
                .max()
                .ok_or(StoreError::Corrupt)?;
            sqlx::query!("UPDATE history_coverage SET pruned_through_ms=MAX(COALESCE(pruned_through_ms,0),?1),pruned_through_id=MAX(pruned_through_id,?2) WHERE singleton=1",last_time,last_id).execute(&mut *tx).await.map_err(StoreError::database)?;
        }
        let count = u64::try_from(removed.len()).map_err(StoreError::corrupt)?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(count)
    }
}
