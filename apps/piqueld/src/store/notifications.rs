//! Durable notification conditions and delivery outbox, driven by committed events.
use super::{Store, StoreError, new_id, now_ms};
use crate::config::{NotificationConfig, WebhookDestination};
use piqueld_core::{
    Event,
    api::Page,
    observability::{
        DeliveryState, Diagnostic, DiagnosticCode, EventScope, NotificationCategory,
        NotificationDelivery,
    },
};
use sha2::{Digest, Sha256};
use sqlx::{Sqlite, Transaction};

impl WebhookDestination {
    /// Hex SHA-256 of the destination's kind and URL. Stored with deliveries and
    /// routes so a destination whose name is kept but URL or kind changes is
    /// treated as a different receiver and never gets the old one's queue.
    pub(crate) fn fingerprint(&self) -> String {
        format!(
            "{:x}",
            Sha256::digest(format!("{:?}:{}", self.kind, self.url.expose()))
        )
    }
}
impl Store {
    /// Reads delivery history without destination credentials or response bodies.
    /// # Errors
    /// Returns pagination or storage errors.
    pub async fn deliveries(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<NotificationDelivery>, StoreError> {
        let fetch = super::page_limit(limit)? + 1;
        let mut rows = sqlx::query!(
            "SELECT id AS \"id!\",event_id,destination,category,state,attempts,created_at_ms,next_attempt_ms,updated_at_ms,last_error
            FROM notification_deliveries
            WHERE ?1 IS NULL OR id<?1
            ORDER BY id DESC LIMIT ?2",
            cursor,
            fetch,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?;
        let more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = more.then(|| rows.last().map(|r| r.id.clone())).flatten();
        Ok(Page {
            items: rows
                .into_iter()
                .map(|r| {
                    Ok(NotificationDelivery {
                        id: r.id,
                        event_id: r.event_id,
                        destination: r.destination,
                        category: NotificationCategory::parse(&r.category)
                            .ok_or(StoreError::Corrupt)?,
                        state: DeliveryState::parse(&r.state).ok_or(StoreError::Corrupt)?,
                        attempts: r.attempts,
                        created_at_ms: r.created_at_ms,
                        next_attempt_ms: r.next_attempt_ms,
                        updated_at_ms: r.updated_at_ms,
                        last_error: r.last_error,
                    })
                })
                .collect::<Result<_, StoreError>>()?,
            next_cursor,
        })
    }
    /// Retries a failed delivery only while its destination/category remain enabled.
    /// Also refuses (as `StoreError::InvalidInput`) failures already followed by a
    /// delivered recovery, recoveries whose incident has reopened, and failures
    /// whose incident has since closed. Resets the delivery to `pending` with a
    /// fresh retry window.
    ///
    /// # Errors
    /// Returns absence, disabled delivery or storage errors.
    pub async fn retry_delivery(&self, id: &str) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let row = sqlx::query!(
            "SELECT destination,category,destination_fingerprint,event_id FROM notification_deliveries WHERE id=?1 AND state='failed'",
            id,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        if !self.notifications.category_enabled(
            NotificationCategory::parse(&row.category).ok_or(StoreError::Corrupt)?,
        ) || !self.notifications.destinations.iter().any(|d| {
            d.enabled && d.name == row.destination && d.fingerprint() == row.destination_fingerprint
        }) {
            return Err(StoreError::InvalidInput);
        }
        // Never replay an old failure after its destination acknowledged recovery.
        let recovered = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM notification_recovery_sources s
            JOIN notification_deliveries recovery ON recovery.id=s.recovery_id
            WHERE s.failure_id=?1 AND recovery.state='delivered'",
            id,
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        if recovered > 0 {
            return Err(StoreError::InvalidInput);
        }
        // A recovery is stale once its incident has opened again.
        if row.category == NotificationCategory::Recovery.as_str() {
            let reopened = sqlx::query_scalar!(
                "SELECT COUNT(*) FROM notification_recovery_sources s
                JOIN notification_conditions c ON c.key=s.condition_key
                WHERE s.recovery_id=?1",
                id,
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(StoreError::database)?;
            if reopened > 0 {
                return Err(StoreError::InvalidInput);
            }
        } else {
            // A closed incident cannot be announced again without a matching recovery.
            let open = sqlx::query_scalar!(
                "SELECT COUNT(*) FROM notification_conditions WHERE event_id=?1",
                row.event_id,
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(StoreError::database)?;
            if open == 0 {
                return Err(StoreError::InvalidInput);
            }
        }
        let now = now_ms();
        sqlx::query!(
            "UPDATE notification_deliveries
            SET state='pending',retry_started_at_ms=?1,next_attempt_ms=?1,updated_at_ms=?1,last_error=NULL
            WHERE id=?2",
            now,
            id,
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }
    /// Reconciles the outbox with the current notification configuration at startup:
    /// 1. cancels pending deliveries whose category or destination is disabled or changed;
    /// 2. drops routes for disabled or changed category/destination pairs;
    /// 3. adds routes for newly enabled pairs, starting after the latest event so
    ///    a new destination is not backfilled with history;
    /// 4. restarts the sustained-failure window of conditions not yet notified.
    pub(crate) async fn configure_deliveries(&self) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let rows = sqlx::query!(
            "SELECT id AS \"id!\",destination,destination_fingerprint,category FROM notification_deliveries WHERE state='pending'"
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let now = now_ms();
        for row in rows {
            if !self.notifications.category_enabled(
                NotificationCategory::parse(&row.category).ok_or(StoreError::Corrupt)?,
            ) || !self.notifications.destinations.iter().any(|d| {
                d.enabled
                    && d.name == row.destination
                    && d.fingerprint() == row.destination_fingerprint
            }) {
                sqlx::query!(
                    "UPDATE notification_deliveries
                    SET state='cancelled',last_error='Disabled or changed by configuration',updated_at_ms=?1
                    WHERE id=?2",
                    now,
                    row.id,
                )
                .execute(&mut *tx)
                .await
                .map_err(StoreError::database)?;
            }
        }
        let routes =
            sqlx::query!("SELECT category,destination,fingerprint FROM notification_routes")
                .fetch_all(&mut *tx)
                .await
                .map_err(StoreError::database)?;
        for route in routes {
            if !self.notifications.category_enabled(
                NotificationCategory::parse(&route.category).ok_or(StoreError::Corrupt)?,
            ) || !self.notifications.destinations.iter().any(|d| {
                d.enabled && d.name == route.destination && d.fingerprint() == route.fingerprint
            }) {
                sqlx::query!(
                    "DELETE FROM notification_routes WHERE category=?1 AND destination=?2",
                    route.category,
                    route.destination,
                )
                .execute(&mut *tx)
                .await
                .map_err(StoreError::database)?;
            }
        }
        for category in self.notifications.enabled_categories() {
            let category = category.as_str();
            for destination in self.notifications.destinations.iter().filter(|d| d.enabled) {
                let fingerprint = destination.fingerprint();
                sqlx::query!(
                    "INSERT INTO notification_routes(category,destination,fingerprint,after_event_id)
                    VALUES(?1,?2,?3,COALESCE((SELECT MAX(id)
                    FROM events),0))
                    ON CONFLICT(category,destination) DO NOTHING",
                    category,
                    destination.name,
                    fingerprint,
                )
                .execute(&mut *tx)
                .await
                .map_err(StoreError::database)?;
            }
        }
        // A process restart interrupts continuous observation; retain open incident deduplication.
        sqlx::query!(
            "UPDATE notification_conditions SET first_seen_ms=?1,last_seen_ms=?1 WHERE notified=0",
            now,
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }
    /// Advances the notification cursor over the next batch (up to 100) of
    /// committed events, opening/closing conditions and enqueueing deliveries.
    /// Returns early without changes if another caller moved the cursor between
    /// the unlocked read and the write transaction.
    pub(crate) async fn process_notifications(&self) -> Result<(), StoreError> {
        // Read events outside the writer transaction, then compare the cursor under the lock.
        let cursor =
            sqlx::query_scalar!("SELECT event_id FROM notification_cursor WHERE singleton=1")
                .fetch_one(&self.pool)
                .await
                .map_err(StoreError::database)?;
        let events = self
            .events(None, Some(&format!("v1:{cursor}")), 100)
            .await?;
        if events.items.is_empty() {
            return Ok(());
        }
        let (_writer, mut tx) = self.begin_immediate().await?;
        let current =
            sqlx::query_scalar!("SELECT event_id FROM notification_cursor WHERE singleton=1")
                .fetch_one(&mut *tx)
                .await
                .map_err(StoreError::database)?;
        if current != cursor {
            return Ok(());
        }
        for event in &events.items {
            // Deletion may have removed an event between the read and this transaction.
            if sqlx::query_scalar!("SELECT id FROM events WHERE id=?1", event.id,)
                .fetch_optional(&mut *tx)
                .await
                .map_err(StoreError::database)?
                .is_none()
            {
                continue;
            }
            self.process_notification_event(&mut tx, event).await?;
        }
        if let Some(event) = events.items.last() {
            sqlx::query!(
                "UPDATE notification_cursor SET event_id=?1 WHERE singleton=1",
                event.id,
            )
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        }
        tx.commit().await.map_err(StoreError::database)
    }
    /// Applies one event to notification state. A successful operation closes the
    /// application's build and deployment failure conditions (queueing
    /// recoveries). Failed operations and daemon diagnostics open a condition and
    /// notify immediately the first time it opens; repeats while it is open are
    /// deduplicated. Dependency outages are skipped here because
    /// `observe_condition` notifies them only once sustained.
    ///
    /// Condition keys:
    /// ```text
    /// <environment_id>:<category>   application build/deployment failures
    /// daemon:<error_code>           daemon failures
    /// ```
    async fn process_notification_event(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        event: &Event,
    ) -> Result<(), StoreError> {
        let app = (event.scope == EventScope::Application)
            .then(|| event.environment_id.as_ref().map(ToString::to_string))
            .flatten();
        if event.kind == "operation_succeeded" {
            for category in [
                NotificationCategory::BuildFailures,
                NotificationCategory::DeploymentFailures,
            ] {
                Self::close_condition(
                    tx,
                    &self.notifications,
                    &format!("{}:{category}", app.as_deref().unwrap_or("daemon")),
                    event.id,
                )
                .await?;
            }
        }
        let category = match (
            event.kind.as_str(),
            event.scope,
            event.error_code.as_deref(),
        ) {
            ("operation_failed", EventScope::Application, Some("git_build_failed")) => {
                NotificationCategory::BuildFailures
            }
            ("operation_failed", EventScope::Application, _) => {
                NotificationCategory::DeploymentFailures
            }
            ("diagnostic" | "operation_failed", EventScope::Daemon, Some(code))
                if !matches!(
                    code,
                    "docker_unavailable"
                        | "swarm_manager_unavailable"
                        | "swarm_topology_unsupported"
                        | "ingress_unavailable"
                ) =>
            {
                NotificationCategory::DaemonFailures
            }
            _ => return Ok(()),
        };
        let key = if event.scope == EventScope::Daemon {
            format!(
                "daemon:{}",
                event.error_code.as_deref().unwrap_or("internal_error")
            )
        } else {
            format!("{}:{category}", app.as_deref().unwrap_or("daemon"))
        };
        let notified = sqlx::query_scalar!(
            "SELECT notified FROM notification_conditions WHERE key=?1",
            key,
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        if notified.is_none() {
            let category_name = category.as_str();
            sqlx::query!(
                "INSERT INTO notification_conditions(key,category,environment_id,event_id,first_seen_ms,last_seen_ms,notified)
                VALUES(?1,?2,?3,?4,?5,?5,1)",
                key,
                category_name,
                app,
                event.id,
                event.created_at_ms,
            )
            .execute(&mut **tx)
            .await
            .map_err(StoreError::database)?;
            Self::enqueue(tx, &self.notifications, event.id, category).await?;
        }
        Ok(())
    }
    /// Queues a delivery of `event` to every enabled destination, if `category` is enabled.
    async fn enqueue(
        tx: &mut Transaction<'_, Sqlite>,
        config: &NotificationConfig,
        event: i64,
        category: NotificationCategory,
    ) -> Result<(), StoreError> {
        if config.category_enabled(category) {
            for destination in config.destinations.iter().filter(|d| d.enabled) {
                Self::enqueue_destination(tx, event, category, destination).await?;
            }
        }
        Ok(())
    }

    /// Queues one delivery idempotently and returns its id (new or existing).
    /// Returns `None` without queueing when the destination has no route for the
    /// category or the event predates the route's `after_event_id`.
    async fn enqueue_destination(
        tx: &mut Transaction<'_, Sqlite>,
        event: i64,
        category: NotificationCategory,
        destination: &WebhookDestination,
    ) -> Result<Option<String>, StoreError> {
        let category = category.as_str();
        let after = sqlx::query_scalar!(
            "SELECT after_event_id FROM notification_routes WHERE category=?1 AND destination=?2",
            category,
            destination.name,
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        if after.is_none_or(|after| event <= after) {
            return Ok(None);
        }
        let now = now_ms();
        let id = new_id("delivery");
        let fingerprint = destination.fingerprint();
        sqlx::query!(
            "INSERT INTO notification_deliveries(
            id,event_id,destination,destination_fingerprint,category,state,
            created_at_ms,retry_started_at_ms,next_attempt_ms,updated_at_ms
            ) VALUES(?1,?2,?3,?4,?5,'pending',?6,?6,?6,?6)
            ON CONFLICT(event_id,destination,category) DO NOTHING",
            id,
            event,
            destination.name,
            fingerprint,
            category,
            now,
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        // Multiple conditions may clear in one event; they share a recovery delivery.
        sqlx::query_scalar!(
            "SELECT id AS \"id!\" FROM notification_deliveries
            WHERE event_id=?1 AND destination=?2 AND category=?3",
            event,
            destination.name,
            category,
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)
    }
    /// Clears an open condition. If it was notified and recoveries are enabled,
    /// queues a recovery for each destination that received (or is still
    /// receiving) the failure, linking them in `notification_recovery_sources`
    /// so the recovery is sent only after the failure is acknowledged.
    async fn close_condition(
        tx: &mut Transaction<'_, Sqlite>,
        config: &NotificationConfig,
        key: &str,
        event: i64,
    ) -> Result<(), StoreError> {
        let condition = sqlx::query!(
            "SELECT notified,category,event_id FROM notification_conditions WHERE key=?1",
            key,
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        if let Some(condition) = condition {
            let category =
                NotificationCategory::parse(&condition.category).ok_or(StoreError::Corrupt)?;
            if condition.notified != 0
                && config.category_enabled(category)
                && config.category_enabled(NotificationCategory::Recovery)
            {
                let failures = sqlx::query!(
                    "SELECT id AS \"id!\",destination,destination_fingerprint
                    FROM notification_deliveries WHERE event_id=?1 AND category=?2
                    AND state IN ('pending','delivered')",
                    condition.event_id,
                    condition.category,
                )
                .fetch_all(&mut **tx)
                .await
                .map_err(StoreError::database)?;
                for failure in failures {
                    let Some(destination) = config.destinations.iter().find(|d| {
                        d.enabled
                            && d.name == failure.destination
                            && d.fingerprint() == failure.destination_fingerprint
                    }) else {
                        continue;
                    };
                    if let Some(recovery) = Self::enqueue_destination(
                        tx,
                        event,
                        NotificationCategory::Recovery,
                        destination,
                    )
                    .await?
                    {
                        sqlx::query!(
                            "INSERT INTO notification_recovery_sources(recovery_id,failure_id,condition_key)
                            VALUES(?1,?2,?3) ON CONFLICT DO NOTHING",
                            recovery,
                            failure.id,
                            key,
                        )
                        .execute(&mut **tx)
                        .await
                        .map_err(StoreError::database)?;
                    }
                }
            }
        }
        sqlx::query!("DELETE FROM notification_conditions WHERE key=?1", key,)
            .execute(&mut **tx)
            .await
            .map_err(StoreError::database)?;
        Ok(())
    }
    /// Writes the health transition in the same transaction as its notification condition.
    /// Inserts a `dependency_health_changed` event (with a diagnostic when
    /// failing) and returns its row id. Daemon keys must be a `DiagnosticCode`.
    async fn record_health_transition(
        tx: &mut Transaction<'_, Sqlite>,
        key: &str,
        application: Option<&str>,
        failed: bool,
        observed: i64,
    ) -> Result<i64, StoreError> {
        let code = if application.is_some() {
            DiagnosticCode::ServiceDegraded
        } else {
            DiagnosticCode::parse(key).ok_or(StoreError::InvalidInput)?
        };
        let subject = match (application, key) {
            (Some(id), _) => format!("Service health for application {id}"),
            (None, "docker_unavailable") => "Docker Engine".into(),
            (None, "swarm_manager_unavailable") => "Swarm manager".into(),
            (None, "ingress_unavailable") => "Managed ingress gateway".into(),
            _ => key.to_owned(),
        };
        let summary = if failed {
            format!("{subject} is unavailable or degraded")
        } else {
            format!("{subject} recovered")
        };
        let scope = if application.is_some() {
            "application"
        } else {
            "daemon"
        };
        let diagnostic =
            failed.then(|| Diagnostic::new(new_id("diagnostic"), code, summary.clone()));
        let diagnostic_id = diagnostic.as_ref().map(|d| d.id.as_str());
        let json = diagnostic
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(StoreError::corrupt)?;
        let error_code = failed.then_some(code.as_str());
        let event = sqlx::query!(
            "INSERT INTO events(scope,application_id,environment_id,kind,message,error_code,diagnostic_id,diagnostic_json,
            created_at_ms) VALUES(?1,(SELECT application_id FROM environments WHERE id=?2),?2,'dependency_health_changed',?3,?4,?5,?6,?7)",
            scope,
            application,
            summary,
            error_code,
            diagnostic_id,
            json,
            observed,
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?
        .last_insert_rowid();
        Ok(event)
    }

    /// Records continuously observed dependency health, emitting events only on transitions.
    /// Ignores observations for deleted applications and ones no newer than the
    /// last. A new failure opens an unnotified `health:<key>` condition that
    /// `notify_sustained_failure` announces once it persists; a recovery closes
    /// it. An initial healthy observation records nothing.
    pub(crate) async fn observe_condition(
        &self,
        key: &str,
        application: Option<&str>,
        failed: bool,
        observed: i64,
        max_gap_ms: i64,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        if let Some(application) = application {
            let exists = sqlx::query_scalar!(
                "SELECT COUNT(*) FROM environments WHERE id=?1 AND delete_intent=0",
                application,
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(StoreError::database)?;
            if exists == 0 {
                return Ok(());
            }
        }
        let previous = sqlx::query!(
            "SELECT failed,observed_at_ms FROM dependency_observations WHERE key=?1",
            key,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        if previous
            .as_ref()
            .is_some_and(|p| p.observed_at_ms >= observed)
        {
            return Ok(());
        }
        let category = if application.is_some() {
            NotificationCategory::ServiceDegradation
        } else {
            NotificationCategory::DaemonFailures
        };
        let changed = previous.as_ref().is_none_or(|p| (p.failed != 0) != failed);
        let gap = previous
            .as_ref()
            .is_none_or(|p| observed.saturating_sub(p.observed_at_ms) > max_gap_ms);
        let condition_key = format!("health:{key}");
        if changed && (failed || previous.is_some()) {
            let event =
                Self::record_health_transition(&mut tx, key, application, failed, observed).await?;
            if failed {
                let category_name = category.as_str();
                sqlx::query!(
                    "INSERT INTO notification_conditions(key,category,environment_id,event_id,first_seen_ms,last_seen_ms)
                    VALUES(?1,?2,?3,?4,?5,?5)
                    ON CONFLICT(key) DO UPDATE
                    SET event_id=excluded.event_id,first_seen_ms=excluded.first_seen_ms,last_seen_ms=excluded.last_seen_ms,
                    notified=0",
                    condition_key,
                    category_name,
                    application,
                    event,
                    observed,
                )
                .execute(&mut *tx)
                .await
                .map_err(StoreError::database)?;
            } else {
                Self::close_condition(&mut tx, &self.notifications, &condition_key, event).await?;
            }
        }
        if failed {
            self.notify_sustained_failure(&mut tx, &condition_key, category, gap, observed)
                .await?;
        }
        sqlx::query!(
            "INSERT INTO dependency_observations(key,failed,observed_at_ms)
            VALUES(?1,?2,?3)
            ON CONFLICT(key) DO UPDATE
            SET failed=excluded.failed,observed_at_ms=excluded.observed_at_ms",
            key,
            failed,
            observed,
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }
    /// Extends an unnotified failure condition to `observed`, restarting its
    /// window after an observation gap, and notifies once it has failed
    /// continuously for `failure_threshold_seconds`.
    async fn notify_sustained_failure(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        condition_key: &str,
        category: NotificationCategory,
        gap: bool,
        observed: i64,
    ) -> Result<(), StoreError> {
        sqlx::query!(
            "UPDATE notification_conditions
            SET first_seen_ms=CASE WHEN ?1 THEN ?2 ELSE first_seen_ms END,last_seen_ms=?2
            WHERE key=?3 AND notified=0",
            gap,
            observed,
            condition_key,
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        let threshold = i64::try_from(
            self.notifications
                .failure_threshold_seconds
                .saturating_mul(1000),
        )
        .unwrap_or(i64::MAX);
        if let Some(condition) = sqlx::query!(
            "SELECT event_id FROM notification_conditions WHERE key=?1 AND notified=0 AND last_seen_ms-first_seen_ms>=?2",
            condition_key,
            threshold,
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)? {
            Self::enqueue(tx, &self.notifications, condition.event_id, category).await?;
            sqlx::query!(
                "UPDATE notification_conditions SET notified=1 WHERE key=?1",
                condition_key,
            )
            .execute(&mut **tx)
            .await
            .map_err(StoreError::database)?;
        }
        Ok(())
    }

    /// Feeds each live environment's recent runtime health into
    /// `observe_condition`. Degraded services, or no running services when some
    /// are expected, count as failing; observations older than `max_gap_ms` are skipped.
    pub(crate) async fn observe_services(&self, max_gap_ms: i64) -> Result<(), StoreError> {
        let rows = sqlx::query!(
            "SELECT s.environment_id AS \"environment_id!\",s.runtime_health,s.health_observed_at_ms,COALESCE(json_array_length(e.resolved_json,
            '$.services'),0) AS \"expected_services!: i64\"
            FROM environment_status s JOIN environments e ON e.id=s.environment_id
            WHERE s.health_observed_at_ms IS NOT NULL AND s.runtime_health IS NOT NULL AND e.delete_intent=0"
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?;
        for row in rows {
            let observed = row.health_observed_at_ms.ok_or(StoreError::Corrupt)?;
            if now_ms().saturating_sub(observed) <= max_gap_ms {
                self.observe_condition(
                    &row.environment_id,
                    Some(&row.environment_id),
                    row.runtime_health.as_deref() == Some("degraded")
                        || (row.runtime_health.as_deref() == Some("absent")
                            && row.expected_services > 0),
                    observed,
                    max_gap_ms,
                )
                .await?;
            }
        }
        Ok(())
    }
    /// Claims the next due delivery for the notification worker:
    /// 1. fails pending deliveries whose retry window expired;
    /// 2. cancels recoveries none of whose failures can still be delivered;
    /// 3. picks the earliest due delivery, holding recoveries until every linked
    ///    failure is settled and at least one was delivered;
    /// 4. leases it for 30 seconds by pushing `next_attempt_ms` and counting the attempt.
    ///
    /// Fails with `StoreError::InvalidInput` if its destination is no longer configured.
    pub(crate) async fn claim_delivery(
        &self,
    ) -> Result<Option<(NotificationDelivery, WebhookDestination)>, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let now = now_ms();
        let expired = now.saturating_sub(
            i64::try_from(self.notifications.retry_window_seconds.saturating_mul(1000))
                .unwrap_or(i64::MAX),
        );
        // A recovery's retry window starts when it can first be sent, not while
        // it is waiting for acknowledgement of the original failure.
        sqlx::query!(
            "UPDATE notification_deliveries
            SET state='failed',last_error='Delivery retry window expired',updated_at_ms=?1
            WHERE state='pending' AND retry_started_at_ms<?2
            AND (category!='recovery' OR attempts>0)",
            now,
            expired,
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        sqlx::query!(
            "UPDATE notification_deliveries
            SET state='cancelled',last_error='No failure notification was acknowledged',updated_at_ms=?1
            WHERE state='pending' AND category='recovery' AND NOT EXISTS (
            SELECT 1 FROM notification_recovery_sources s
            JOIN notification_deliveries failure ON failure.id=s.failure_id
            WHERE s.recovery_id=notification_deliveries.id
            AND failure.state IN ('pending','delivered')
            )",
            now,
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let row = sqlx::query!(
            "SELECT id AS \"id!\",destination,destination_fingerprint,event_id,category,attempts,
            created_at_ms,updated_at_ms,last_error
            FROM notification_deliveries d
            WHERE state='pending' AND next_attempt_ms<=?1
            AND (category!='recovery' OR (
            EXISTS (SELECT 1 FROM notification_recovery_sources s
            JOIN notification_deliveries f ON f.id=s.failure_id
            WHERE s.recovery_id=d.id AND f.state='delivered')
            AND NOT EXISTS (SELECT 1 FROM notification_recovery_sources s
            JOIN notification_deliveries f ON f.id=s.failure_id
            WHERE s.recovery_id=d.id AND f.state='pending')
            ))
            ORDER BY next_attempt_ms,id LIMIT 1",
            now,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let Some(row) = row else {
            // Expiration/cancellation still needs committing when nothing is due.
            tx.commit().await.map_err(StoreError::database)?;
            return Ok(None);
        };
        let category = NotificationCategory::parse(&row.category).ok_or(StoreError::Corrupt)?;
        let destination = self
            .notifications
            .destinations
            .iter()
            .find(|d| {
                d.enabled
                    && d.name == row.destination
                    && d.fingerprint() == row.destination_fingerprint
                    && self.notifications.category_enabled(category)
            })
            .cloned()
            .ok_or(StoreError::InvalidInput)?;
        let lease = now.saturating_add(30_000);
        sqlx::query!(
            "UPDATE notification_deliveries
            SET retry_started_at_ms=CASE WHEN category='recovery' AND attempts=0 THEN ?2 ELSE retry_started_at_ms END,
            attempts=attempts+1,next_attempt_ms=?1,updated_at_ms=?2 WHERE id=?3",
            lease,
            now,
            row.id,
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(Some((
            NotificationDelivery {
                id: row.id,
                event_id: row.event_id,
                destination: row.destination,
                category,
                state: DeliveryState::Pending,
                attempts: row.attempts + 1,
                created_at_ms: row.created_at_ms,
                next_attempt_ms: lease,
                updated_at_ms: now,
                last_error: row.last_error,
            },
            destination,
        )))
    }

    /// Records the result of a claimed attempt. Only pending deliveries change;
    /// `delay` (seconds) schedules the next attempt when `state` stays `Pending`.
    pub(crate) async fn complete_delivery(
        &self,
        id: &str,
        state: DeliveryState,
        error: Option<&str>,
        delay: u64,
    ) -> Result<(), StoreError> {
        let _writer = self.writers.lock().await;
        let now = now_ms();
        let next =
            now.saturating_add(i64::try_from(delay.saturating_mul(1000)).unwrap_or(i64::MAX));
        let state = state.as_str();
        sqlx::query!(
            "UPDATE notification_deliveries
            SET state=?1,last_error=?2,next_attempt_ms=?3,updated_at_ms=?4
            WHERE id=?5 AND state='pending'",
            state,
            error,
            next,
            now,
            id,
        )
        .execute(&self.pool)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }
}
