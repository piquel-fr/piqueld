//! Immutable diagnostic history, independent of operation retention.
use super::access::{Visibility, scope_json};
use super::{ApplicationId, EnvironmentId, Store, StoreError, new_id, now_ms, page_limit};
use piqueld_core::{
    Event,
    api::Page,
    observability::{Diagnostic, EventFilter, EventScope},
};
use sqlx::{QueryBuilder, Sqlite, Transaction};

impl Store {
    /// Records an event of `kind` copying the operation's current context
    /// (environment, generation, attempt, error, phase, resource).
    /// For a failed operation it attaches a diagnostic, reusing the one already
    /// recorded for the same attempt and error code so repeated events share one
    /// occurrence ID.
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
            let prior = sqlx::query_scalar!(
                "SELECT diagnostic_json
                FROM events
                WHERE operation_id=?1 AND attempt=?2 AND error_code=?3 AND diagnostic_json IS NOT NULL
                ORDER BY id DESC LIMIT 1",
                id,
                attempt,
                code,
            )
            .fetch_optional(&mut **tx)
            .await
            .map_err(StoreError::database)?.flatten();
            Some(match prior {
                Some(json) => serde_json::from_str(&json).map_err(StoreError::corrupt)?,
                None => Diagnostic::from_recorded_code(
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
        sqlx::query!(
            "INSERT INTO events(application_id,environment_id,operation_id,generation,attempt,kind,message,
            error_code,phase,resource,created_at_ms,scope,diagnostic_id,diagnostic_json)
            SELECT e.application_id,o.environment_id,o.id,o.generation,o.attempt,?1,?2,o.error_code,o.phase,
            o.resource,?3,?4,?5,?6
            FROM operations o LEFT JOIN environments e ON e.id=o.environment_id
            WHERE o.id=?7",
            kind,
            message,
            now,
            scope,
            diagnostic_id,
            json,
            id,
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// Reads legacy oldest-first event pages from every scope.
    /// # Errors
    /// Returns storage or pagination errors.
    pub async fn events(
        &self,
        application: Option<&EnvironmentId>,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<Event>, StoreError> {
        self.filtered_events(
            &EventFilter {
                environment_id: application.map(ToString::to_string),
                ..EventFilter::default()
            },
            &Visibility::ALL,
            cursor,
            limit,
        )
        .await
    }

    /// Reads indexed, independently understandable history in either
    /// direction, limited to what `visible` allows.
    /// # Errors
    /// Returns storage, decoding or invalid selection errors.
    pub async fn filtered_events(
        &self,
        filter: &EventFilter,
        visible: &Visibility,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<Event>, StoreError> {
        let fetch = page_limit(limit)? + 1;
        let cursor = cursor
            .map(Self::event_cursor)
            .transpose()?
            .unwrap_or(if filter.descending { i64::MAX } else { 0 });
        validate_filter(filter)?;
        let mut query = EventRow::query(filter, visible, cursor, fetch)?;
        let mut rows = query
            .build_query_as::<EventRow>()
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::database)?;
        let more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = more
            .then(|| rows.last().map(|r| format!("v1:{}", r.id)))
            .flatten();
        let items = rows
            .into_iter()
            .map(EventRow::into_event)
            .collect::<Result<_, StoreError>>()?;
        Ok(Page { items, next_cursor })
    }
    /// Reads the next oldest-first stream batch after `after` from one snapshot.
    /// The returned checkpoint covers every scanned event, including those the
    /// filter excluded, so a quiet filtered stream still advances past pruning.
    /// # Errors
    /// Returns expired history, storage, decoding or invalid selection errors.
    pub async fn stream_events(
        &self,
        filter: &EventFilter,
        visible: &Visibility,
        after: i64,
        limit: usize,
    ) -> Result<(Vec<Event>, i64), StoreError> {
        let fetch = page_limit(limit)? + 1;
        validate_filter(filter)?;
        let mut tx = self.pool.begin().await.map_err(StoreError::database)?;
        let coverage = sqlx::query!(
            r#"SELECT pruned_through_id AS "pruned!: i64",
            (SELECT COALESCE(MAX(id),0) FROM events) AS "latest!: i64"
            FROM history_coverage WHERE singleton=1"#
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        if after > 0 && after < coverage.pruned {
            return Err(StoreError::HistoryExpired);
        }
        let mut rows = EventRow::query(filter, visible, after, fetch)?
            .build_query_as::<EventRow>()
            .fetch_all(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)?;
        let more = rows.len() > limit;
        rows.truncate(limit);
        let checkpoint = match rows.last() {
            Some(row) if more => row.id,
            _ => coverage.latest.max(after),
        };
        let items = rows
            .into_iter()
            .map(EventRow::into_event)
            .collect::<Result<_, StoreError>>()?;
        Ok((items, checkpoint))
    }
    /// Parses an event page cursor into the last event ID already returned.
    ///
    /// ```text
    /// "v1:42" -> 42
    /// "42" | "v1:-1" -> InvalidInput
    /// ```
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
            id,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        self.event(row.id).await
    }
    /// Records an unexpected failure independently of any operation, caused
    /// by `actor`'s request if any.
    /// # Errors
    /// Returns the underlying journal failure; callers must retain structured fallback logs.
    pub async fn record_diagnostic(
        &self,
        diagnostic: &Diagnostic,
        request_id: Option<&str>,
        application: Option<&EnvironmentId>,
        actor: super::Attribution<'_>,
    ) -> Result<(), StoreError> {
        let _writer = self.writers.lock().await;
        let mut diagnostic = diagnostic.clone();
        if application.is_none() {
            diagnostic.scope = EventScope::Daemon;
        }
        let scope = diagnostic.scope.as_str();
        let app = application.map(EnvironmentId::as_str);
        let json = serde_json::to_string(&diagnostic).map_err(StoreError::corrupt)?;
        let now = now_ms();
        sqlx::query!(
            "INSERT INTO events(scope,application_id,environment_id,kind,message,error_code,diagnostic_id,
            diagnostic_json,request_id,created_at_ms,actor_user_id,actor_credential_id)
            SELECT ?1,(SELECT application_id FROM environments WHERE id=?2),?2,'diagnostic',?3,?4,?5,?6,?7,?8,?9,?10
            WHERE ?1='daemon' OR EXISTS(SELECT 1
            FROM environments
            WHERE id=?2)",
            scope,
            app,
            diagnostic.summary,
            diagnostic.code,
            diagnostic.id,
            json,
            request_id,
            now,
            actor.user_id,
            actor.credential_id,
        )
        .execute(&self.pool)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// Records an informational environment event outside any operation,
    /// caused by `actor`. The event stays in `application`'s history even if
    /// the environment has been deleted meanwhile, and is skipped once the
    /// application itself is gone, since its history has been removed.
    /// # Errors
    /// Returns storage errors.
    pub(crate) async fn record_environment_event(
        &self,
        actor: super::Attribution<'_>,
        application: &ApplicationId,
        environment: &EnvironmentId,
        kind: &str,
        message: &str,
        resource: &str,
    ) -> Result<(), StoreError> {
        let _writer = self.writers.lock().await;
        let (application, environment) = (application.as_str(), environment.as_str());
        let now = now_ms();
        sqlx::query!(
            "INSERT INTO events(application_id,environment_id,kind,message,resource,created_at_ms,
            actor_user_id,actor_credential_id)
            SELECT ?1,?2,?3,?4,?5,?6,?7,?8 WHERE EXISTS(SELECT 1 FROM applications WHERE id=?1)",
            application,
            environment,
            kind,
            message,
            resource,
            now,
            actor.user_id,
            actor.credential_id,
        )
        .execute(&self.pool)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// Logs a control-plane failure and records it in the journal, falling back
    /// to the daemon log alone when the journal write fails.
    pub(crate) async fn report_diagnostic(
        &self,
        diagnostic: &Diagnostic,
        application: Option<&EnvironmentId>,
    ) {
        tracing::error!(diagnostic_id=%diagnostic.id, code=%diagnostic.code, summary=%diagnostic.summary, "control-plane failure");
        let daemon = super::Attribution::default();
        if let Err(error) = self
            .record_diagnostic(diagnostic, None, application, daemon)
            .await
        {
            tracing::error!(diagnostic_id=%diagnostic.id,error=?error,"diagnostic journal unavailable; occurrence retained in daemon logs");
        }
    }

    /// Prunes application-scoped events. Daemon history has a separate policy.
    /// # Errors
    /// Returns storage errors.
    pub async fn prune_events(&self, cutoff_ms: i64) -> Result<u64, StoreError> {
        self.prune_scope(cutoff_ms, EventScope::Application).await
    }
    /// Prunes daemon-scoped events past the configured retention; zero days
    /// disables pruning.
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
    /// Deletes events in `scope` older than `cutoff`, except those still needed
    /// by open notification conditions, pending deliveries or recoveries, and
    /// active actions. Advances `history_coverage` so streams resuming before the
    /// pruned point get `HistoryExpired`. Returns the number of deleted events.
    async fn prune_scope(&self, cutoff: i64, scope: EventScope) -> Result<u64, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let scope = scope.as_str();
        // Open incidents need their source and delivery history until recovery,
        // even after every failure delivery has been acknowledged.
        let removed = sqlx::query!(
            "DELETE
            FROM events
            WHERE scope=?1 AND created_at_ms<?2 AND NOT EXISTS(SELECT 1
            FROM notification_conditions c
            WHERE c.event_id=events.id) AND NOT EXISTS(SELECT 1
            FROM notification_deliveries d
            WHERE d.event_id=events.id AND d.state='pending') AND NOT EXISTS(SELECT 1
            FROM notification_deliveries failure JOIN notification_recovery_sources s ON s.failure_id=failure.id JOIN notification_deliveries recovery ON recovery.id=s.recovery_id
            WHERE failure.event_id=events.id AND recovery.state='pending') AND NOT EXISTS(SELECT 1
            FROM active_actions a
            WHERE a.id=events.action_id)
            RETURNING id,created_at_ms",
            scope,
            cutoff,
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        if let Some(last_id) = removed.iter().map(|r| r.id).max() {
            let last_time = removed
                .iter()
                .map(|r| r.created_at_ms)
                .max()
                .ok_or(StoreError::Corrupt)?;
            sqlx::query!(
                "UPDATE history_coverage
                SET pruned_through_ms=MAX(COALESCE(pruned_through_ms,0),?1),pruned_through_id=MAX(pruned_through_id,?2)
                WHERE singleton=1",
                last_time,
                last_id,
            )
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        }
        let count = u64::try_from(removed.len()).map_err(StoreError::corrupt)?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(count)
    }
}

/// Rejects inverted time ranges and malformed application or environment IDs
/// in an event filter.
fn validate_filter(filter: &EventFilter) -> Result<(), StoreError> {
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
    if let Some(id) = &filter.environment_id {
        EnvironmentId::parse(id).map_err(StoreError::invalid_input)?;
    }
    Ok(())
}

/// Raw `events` row, decoded into an `Event` by `into_event`.
#[derive(sqlx::FromRow)]
struct EventRow {
    id: i64,
    application_id: Option<String>,
    environment_id: Option<String>,
    operation_id: Option<String>,
    generation: Option<i64>,
    attempt: Option<i64>,
    kind: String,
    message: Option<String>,
    error_code: Option<String>,
    phase: Option<String>,
    resource: Option<String>,
    created_at_ms: i64,
    scope: String,
    action_id: Option<String>,
    retry: Option<i64>,
    retry_delay_ms: Option<i64>,
    duration_ms: Option<i64>,
    request_id: Option<String>,
    diagnostic_json: Option<String>,
    actor_user_id: Option<String>,
    actor_credential_id: Option<String>,
}

impl EventRow {
    /// Builds a keyset-paginated event query after (or, descending, before)
    /// `cursor`, fetching `fetch` rows that `visible` allows.
    /// Optional predicates are assembled from fixed column names; every value is bound.
    /// Direct ordering lets `SQLite` stop after one page instead of sorting retained history.
    fn query<'a>(
        filter: &'a EventFilter,
        visible: &Visibility,
        cursor: i64,
        fetch: i64,
    ) -> Result<QueryBuilder<'a, Sqlite>, StoreError> {
        let mut query = QueryBuilder::new(
            "SELECT id, application_id, environment_id, operation_id, generation, attempt, kind, message, error_code, \
             phase, resource, created_at_ms, scope, action_id, retry, retry_delay_ms, duration_ms, \
             request_id, diagnostic_json, actor_user_id, actor_credential_id FROM events WHERE id",
        );
        query
            .push(if filter.descending { " < " } else { " > " })
            .push_bind(cursor);
        for (column, value) in [
            ("application_id", filter.application_id.as_deref()),
            ("environment_id", filter.environment_id.as_deref()),
            ("operation_id", filter.operation_id.as_deref()),
            ("action_id", filter.action_id.as_deref()),
            ("kind", filter.kind.as_deref()),
            ("error_code", filter.error_code.as_deref()),
            ("scope", filter.scope.map(EventScope::as_str)),
        ] {
            if let Some(value) = value {
                query
                    .push(" AND ")
                    .push(column)
                    .push(" = ")
                    .push_bind(value);
            }
        }
        if let Some(attempt) = filter.attempt {
            query
                .push(" AND attempt = ")
                .push_bind(i64::try_from(attempt).map_err(StoreError::invalid_input)?);
        }
        if let Some(since) = filter.since_ms {
            query.push(" AND created_at_ms >= ").push_bind(since);
        }
        if let Some(until) = filter.until_ms {
            query.push(" AND created_at_ms <= ").push_bind(until);
        }
        if filter.errors_only {
            query.push(" AND error_code IS NOT NULL");
        }
        if let Some(applications) = scope_json(&visible.applications) {
            query
                .push(" AND ((scope = 'application' AND application_id IN (SELECT value FROM json_each(")
                .push_bind(applications)
                .push(")))")
                .push(if visible.daemon {
                    " OR scope = 'daemon')"
                } else {
                    ")"
                });
        } else if !visible.daemon {
            query.push(" AND scope = 'application'");
        }
        query
            .push(if filter.descending {
                " ORDER BY id DESC LIMIT "
            } else {
                " ORDER BY id ASC LIMIT "
            })
            .push_bind(fetch);
        Ok(query)
    }

    /// Converts raw columns into an `Event`, reporting out-of-range values or
    /// unknown scopes as corruption.
    fn into_event(self) -> Result<Event, StoreError> {
        Ok(Event {
            id: self.id,
            application_id: self
                .application_id
                .map(ApplicationId::parse)
                .transpose()
                .map_err(StoreError::corrupt)?,
            environment_id: self
                .environment_id
                .map(EnvironmentId::parse)
                .transpose()
                .map_err(StoreError::corrupt)?,
            operation_id: self.operation_id,
            generation: self
                .generation
                .map(u64::try_from)
                .transpose()
                .map_err(StoreError::corrupt)?,
            attempt: self
                .attempt
                .map(u64::try_from)
                .transpose()
                .map_err(StoreError::corrupt)?,
            kind: self.kind,
            message: self.message,
            error_code: self.error_code,
            phase: self.phase,
            resource: self.resource,
            created_at_ms: self.created_at_ms,
            scope: match self.scope.as_str() {
                "daemon" => EventScope::Daemon,
                "application" => EventScope::Application,
                _ => return Err(StoreError::Corrupt),
            },
            action_id: self.action_id,
            retry: self
                .retry
                .map(u64::try_from)
                .transpose()
                .map_err(StoreError::corrupt)?,
            retry_delay_ms: self
                .retry_delay_ms
                .map(u64::try_from)
                .transpose()
                .map_err(StoreError::corrupt)?,
            duration_ms: self
                .duration_ms
                .map(u64::try_from)
                .transpose()
                .map_err(StoreError::corrupt)?,
            request_id: self.request_id,
            actor_user_id: self.actor_user_id,
            actor_credential_id: self.actor_credential_id,
            diagnostic: self
                .diagnostic_json
                .map(|json| serde_json::from_str(&json))
                .transpose()
                .map_err(StoreError::corrupt)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use piqueld_core::manifest::ApplicationTemplate;
    use sqlx::{Execute, Row};

    #[tokio::test]
    async fn environment_events_keep_their_application_until_it_is_deleted() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("db")).await.unwrap();
        let app = piqueld_core::parse_toml(include_str!(
            "../../../../crates/piqueld-core/tests/fixtures/manifests/prebuilt.toml"
        ))
        .unwrap()
        .normalize(ApplicationId::parse("app-event-test").unwrap());
        store
            .save_application(&ApplicationTemplate::from(&app), None, None)
            .await
            .unwrap();
        // An environment deleted while a command was running.
        let deleted = EnvironmentId::parse("env-deleted").unwrap();
        let gone = ApplicationId::parse("app-deleted").unwrap();
        for application in [app.id(), &gone] {
            store
                .record_environment_event(
                    crate::store::Attribution::default(),
                    application,
                    &deleted,
                    "command_finished",
                    "done",
                    "web",
                )
                .await
                .unwrap();
        }
        let history = store
            .filtered_events(
                &EventFilter {
                    environment_id: Some(deleted.to_string()),
                    ..EventFilter::default()
                },
                &Visibility::ALL,
                None,
                10,
            )
            .await
            .unwrap()
            .items;
        assert_eq!(
            history
                .iter()
                .map(|event| event.application_id.as_ref())
                .collect::<Vec<_>>(),
            [Some(app.id())]
        );
    }

    #[tokio::test]
    async fn history_pages_use_indexes_without_sorting_retained_events() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("db")).await.unwrap();
        for descending in [false, true] {
            for (filter, index) in [
                (EventFilter::default(), "INTEGER PRIMARY KEY"),
                (
                    EventFilter {
                        application_id: Some("app-query".into()),
                        ..Default::default()
                    },
                    "event_application",
                ),
                (
                    EventFilter {
                        environment_id: Some("app-query".into()),
                        ..Default::default()
                    },
                    "event_environment",
                ),
                (
                    EventFilter {
                        operation_id: Some("op-query".into()),
                        ..Default::default()
                    },
                    "event_operation",
                ),
                (
                    EventFilter {
                        action_id: Some("action-query".into()),
                        ..Default::default()
                    },
                    "event_action",
                ),
            ] {
                let filter = EventFilter {
                    descending,
                    ..filter
                };
                let mut query = EventRow::query(
                    &filter,
                    &Visibility::ALL,
                    if descending { i64::MAX } else { 0 },
                    51,
                )
                .unwrap();
                let mut query = query.build();
                let explain = format!("EXPLAIN QUERY PLAN {}", query.sql());
                let args = query.take_arguments().unwrap().unwrap();
                let rows = sqlx::query_with(&explain, args)
                    .fetch_all(&store.pool)
                    .await
                    .unwrap();
                let plan = rows
                    .iter()
                    .map(|row| row.get::<String, _>("detail"))
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(plan.contains(index), "{plan}");
                assert!(!plan.contains("TEMP B-TREE"), "{plan}");
            }
        }
    }
}
