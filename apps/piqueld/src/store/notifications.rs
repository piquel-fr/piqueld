//! Durable notification conditions and delivery outbox, driven by committed events.
use super::{Store, StoreError, new_id, now_ms};
use crate::config::{NotificationConfig, WebhookDestination};
use piqueld_core::{
    Event,
    api::Page,
    observability::{Diagnostic, EventScope, NotificationDelivery},
};
use sha2::{Digest, Sha256};
use sqlx::{Sqlite, Transaction};

impl WebhookDestination {
    pub(crate) fn fingerprint(&self) -> String {
        format!(
            "{:x}",
            Sha256::digest(format!("{:?}:{}", self.kind, self.url))
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
        let mut rows=sqlx::query!("SELECT id AS \"id!\",event_id,destination,category,state,attempts,created_at_ms,next_attempt_ms,updated_at_ms,last_error FROM notification_deliveries WHERE ?1 IS NULL OR id<?1 ORDER BY id DESC LIMIT ?2",cursor,fetch).fetch_all(&self.pool).await.map_err(StoreError::database)?;
        let more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = more.then(|| rows.last().map(|r| r.id.clone())).flatten();
        Ok(Page {
            items: rows
                .into_iter()
                .map(|r| NotificationDelivery {
                    id: r.id,
                    event_id: r.event_id,
                    destination: r.destination,
                    category: r.category,
                    state: r.state,
                    attempts: r.attempts,
                    created_at_ms: r.created_at_ms,
                    next_attempt_ms: r.next_attempt_ms,
                    updated_at_ms: r.updated_at_ms,
                    last_error: r.last_error,
                })
                .collect(),
            next_cursor,
        })
    }
    /// Retries a failed delivery only while its destination/category remain enabled.
    /// # Errors
    /// Returns absence, disabled delivery or storage errors.
    pub async fn retry_delivery(&self, id: &str) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let row=sqlx::query!("SELECT destination,category,destination_fingerprint FROM notification_deliveries WHERE id=?1 AND state='failed'",id).fetch_optional(&mut *tx).await.map_err(StoreError::database)?.ok_or(StoreError::NotFound)?;
        if !self.notifications.category_enabled(&row.category)
            || !self.notifications.destinations.iter().any(|d| {
                d.enabled
                    && d.name == row.destination
                    && d.fingerprint() == row.destination_fingerprint
            })
        {
            return Err(StoreError::InvalidInput);
        }
        let now = now_ms();
        sqlx::query!("UPDATE notification_deliveries SET state='pending',retry_started_at_ms=?1,next_attempt_ms=?1,updated_at_ms=?1,last_error=NULL WHERE id=?2",now,id).execute(&mut *tx).await.map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }
    pub(crate) async fn configure_deliveries(&self) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let rows=sqlx::query!("SELECT id AS \"id!\",destination,destination_fingerprint,category FROM notification_deliveries WHERE state='pending'").fetch_all(&mut *tx).await.map_err(StoreError::database)?;
        let now = now_ms();
        for row in rows {
            if !self.notifications.category_enabled(&row.category)
                || !self.notifications.destinations.iter().any(|d| {
                    d.enabled
                        && d.name == row.destination
                        && d.fingerprint() == row.destination_fingerprint
                })
            {
                sqlx::query!("UPDATE notification_deliveries SET state='cancelled',last_error='Disabled or changed by configuration',updated_at_ms=?1 WHERE id=?2",now,row.id).execute(&mut *tx).await.map_err(StoreError::database)?;
            }
        }
        let routes =
            sqlx::query!("SELECT category,destination,fingerprint FROM notification_routes")
                .fetch_all(&mut *tx)
                .await
                .map_err(StoreError::database)?;
        for route in routes {
            if !self.notifications.category_enabled(&route.category)
                || !self.notifications.destinations.iter().any(|d| {
                    d.enabled && d.name == route.destination && d.fingerprint() == route.fingerprint
                })
            {
                sqlx::query!(
                    "DELETE FROM notification_routes WHERE category=?1 AND destination=?2",
                    route.category,
                    route.destination
                )
                .execute(&mut *tx)
                .await
                .map_err(StoreError::database)?;
            }
        }
        for category in self.notifications.enabled_categories() {
            for destination in self.notifications.destinations.iter().filter(|d| d.enabled) {
                let fingerprint = destination.fingerprint();
                sqlx::query!("INSERT INTO notification_routes(category,destination,fingerprint,after_event_id) VALUES(?1,?2,?3,COALESCE((SELECT MAX(id) FROM events),0)) ON CONFLICT(category,destination) DO NOTHING",category,destination.name,fingerprint).execute(&mut *tx).await.map_err(StoreError::database)?;
            }
        }
        // A process restart interrupts continuous observation; retain open incident deduplication.
        sqlx::query!(
            "UPDATE notification_conditions SET first_seen_ms=?1,last_seen_ms=?1 WHERE notified=0",
            now
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }
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
            if sqlx::query_scalar!("SELECT id FROM events WHERE id=?1", event.id)
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
                event.id
            )
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        }
        tx.commit().await.map_err(StoreError::database)
    }
    async fn process_notification_event(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        event: &Event,
    ) -> Result<(), StoreError> {
        let app = (event.scope == EventScope::Application)
            .then(|| event.application_id.as_ref().map(ToString::to_string))
            .flatten();
        if event.kind == "operation_succeeded" {
            for category in ["build_failures", "deployment_failures"] {
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
                "build_failures"
            }
            ("operation_failed", EventScope::Application, _) => "deployment_failures",
            ("diagnostic" | "operation_failed", EventScope::Daemon, Some(code))
                if !matches!(
                    code,
                    "docker_unavailable"
                        | "swarm_manager_unavailable"
                        | "swarm_topology_unsupported"
                ) =>
            {
                "daemon_failures"
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
            key
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        if notified.is_none() {
            sqlx::query!("INSERT INTO notification_conditions(key,category,application_id,event_id,first_seen_ms,last_seen_ms,notified) VALUES(?1,?2,?3,?4,?5,?5,1)",key,category,app,event.id,event.created_at_ms).execute(&mut **tx).await.map_err(StoreError::database)?;
            Self::enqueue(tx, &self.notifications, event.id, category).await?;
        }
        Ok(())
    }
    async fn enqueue(
        tx: &mut Transaction<'_, Sqlite>,
        config: &NotificationConfig,
        event: i64,
        category: &str,
    ) -> Result<(), StoreError> {
        if !config.category_enabled(category) {
            return Ok(());
        }
        let now = now_ms();
        for destination in config.destinations.iter().filter(|d| d.enabled) {
            let route=sqlx::query_scalar!("SELECT after_event_id FROM notification_routes WHERE category=?1 AND destination=?2",category,destination.name).fetch_optional(&mut **tx).await.map_err(StoreError::database)?;
            if route.is_none_or(|after| event <= after) {
                continue;
            }
            let id = new_id("delivery");
            let fingerprint = destination.fingerprint();
            sqlx::query!("INSERT INTO notification_deliveries(id,event_id,destination,destination_fingerprint,category,state,created_at_ms,retry_started_at_ms,next_attempt_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,'pending',?6,?6,?6,?6) ON CONFLICT(event_id,destination,category) DO NOTHING",id,event,destination.name,fingerprint,category,now).execute(&mut **tx).await.map_err(StoreError::database)?;
        }
        Ok(())
    }
    async fn close_condition(
        tx: &mut Transaction<'_, Sqlite>,
        config: &NotificationConfig,
        key: &str,
        event: i64,
    ) -> Result<(), StoreError> {
        let condition = sqlx::query!(
            "SELECT notified,category,event_id FROM notification_conditions WHERE key=?1",
            key
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        if let Some(condition) =
            condition.filter(|r| r.notified != 0 && config.category_enabled(&r.category))
        {
            let sent=sqlx::query_scalar!("SELECT COUNT(*) FROM notification_deliveries WHERE event_id=?1 AND category=?2 AND state IN ('pending','delivered','failed')",condition.event_id,condition.category).fetch_one(&mut **tx).await.map_err(StoreError::database)?;
            if sent > 0 {
                Self::enqueue(tx, config, event, "recovery").await?;
            }
        }
        sqlx::query!("DELETE FROM notification_conditions WHERE key=?1", key)
            .execute(&mut **tx)
            .await
            .map_err(StoreError::database)?;
        Ok(())
    }
    /// Records continuously observed dependency health, emitting events only on transitions.
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
                "SELECT COUNT(*) FROM applications WHERE id=?1 AND delete_intent=0",
                application
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
            key
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
            "service_degradation"
        } else {
            "daemon_failures"
        };
        let changed = previous.as_ref().is_none_or(|p| (p.failed != 0) != failed);
        let gap = previous
            .as_ref()
            .is_none_or(|p| observed.saturating_sub(p.observed_at_ms) > max_gap_ms);
        let condition_key = format!("health:{key}");
        if changed && (failed || previous.is_some()) {
            let code = if application.is_some() {
                "service_degraded"
            } else {
                key
            };
            let subject = match (application, key) {
                (Some(id), _) => format!("Service health for application {id}"),
                (None, "docker_unavailable") => "Docker Engine".into(),
                (None, "swarm_manager_unavailable") => "Swarm manager".into(),
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
            let error_code = failed.then_some(code);
            let event=sqlx::query!("INSERT INTO events(scope,application_id,kind,message,error_code,diagnostic_id,diagnostic_json,created_at_ms) VALUES(?1,?2,'dependency_health_changed',?3,?4,?5,?6,?7)",scope,application,summary,error_code,diagnostic_id,json,observed).execute(&mut *tx).await.map_err(StoreError::database)?.last_insert_rowid();
            if failed {
                sqlx::query!("INSERT INTO notification_conditions(key,category,application_id,event_id,first_seen_ms,last_seen_ms) VALUES(?1,?2,?3,?4,?5,?5) ON CONFLICT(key) DO UPDATE SET event_id=excluded.event_id,first_seen_ms=excluded.first_seen_ms,last_seen_ms=excluded.last_seen_ms,notified=0",condition_key,category,application,event,observed).execute(&mut *tx).await.map_err(StoreError::database)?;
            } else {
                Self::close_condition(&mut tx, &self.notifications, &condition_key, event).await?;
            }
        }
        if failed {
            sqlx::query!("UPDATE notification_conditions SET first_seen_ms=CASE WHEN ?1 THEN ?2 ELSE first_seen_ms END,last_seen_ms=?2 WHERE key=?3 AND notified=0",gap,observed,condition_key).execute(&mut *tx).await.map_err(StoreError::database)?;
            let threshold = i64::try_from(
                self.notifications
                    .failure_threshold_seconds
                    .saturating_mul(1000),
            )
            .unwrap_or(i64::MAX);
            if let Some(condition)=sqlx::query!("SELECT event_id FROM notification_conditions WHERE key=?1 AND notified=0 AND last_seen_ms-first_seen_ms>=?2",condition_key,threshold).fetch_optional(&mut *tx).await.map_err(StoreError::database)? {
                Self::enqueue(&mut tx,&self.notifications,condition.event_id,category).await?;
                sqlx::query!("UPDATE notification_conditions SET notified=1 WHERE key=?1",condition_key).execute(&mut *tx).await.map_err(StoreError::database)?;
            }
        }
        sqlx::query!("INSERT INTO dependency_observations(key,failed,observed_at_ms) VALUES(?1,?2,?3) ON CONFLICT(key) DO UPDATE SET failed=excluded.failed,observed_at_ms=excluded.observed_at_ms",key,failed,observed).execute(&mut *tx).await.map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }
    pub(crate) async fn observe_services(&self, max_gap_ms: i64) -> Result<(), StoreError> {
        let rows=sqlx::query!("SELECT s.application_id AS \"application_id!\",s.runtime_health,s.health_observed_at_ms,COALESCE(json_array_length(a.resolved_json,'$.services'),0) AS \"expected_services!: i64\" FROM application_status s JOIN applications a ON a.id=s.application_id WHERE s.health_observed_at_ms IS NOT NULL AND s.runtime_health IS NOT NULL AND a.delete_intent=0").fetch_all(&self.pool).await.map_err(StoreError::database)?;
        for row in rows {
            let observed = row.health_observed_at_ms.ok_or(StoreError::Corrupt)?;
            if now_ms().saturating_sub(observed) <= max_gap_ms {
                self.observe_condition(
                    &row.application_id,
                    Some(&row.application_id),
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
    pub(crate) async fn claim_delivery(
        &self,
    ) -> Result<Option<(NotificationDelivery, WebhookDestination)>, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let now = now_ms();
        let expired = now.saturating_sub(
            i64::try_from(self.notifications.retry_window_seconds.saturating_mul(1000))
                .unwrap_or(i64::MAX),
        );
        sqlx::query!("UPDATE notification_deliveries SET state='failed',last_error='Delivery retry window expired',updated_at_ms=?1 WHERE state='pending' AND retry_started_at_ms<?2",now,expired).execute(&mut *tx).await.map_err(StoreError::database)?;
        let Some(row)=sqlx::query!("SELECT id AS \"id!\",destination,event_id,category,attempts,created_at_ms,next_attempt_ms,updated_at_ms,last_error FROM notification_deliveries WHERE state='pending' AND next_attempt_ms<=?1 ORDER BY next_attempt_ms,id LIMIT 1",now).fetch_optional(&mut *tx).await.map_err(StoreError::database)? else{return Ok(None);};
        let Some(destination) = self
            .notifications
            .destinations
            .iter()
            .find(|d| {
                d.enabled
                    && d.name == row.destination
                    && self.notifications.category_enabled(&row.category)
            })
            .cloned()
        else {
            return Ok(None);
        };
        let lease = now.saturating_add(30_000);
        sqlx::query!("UPDATE notification_deliveries SET attempts=attempts+1,next_attempt_ms=?1,updated_at_ms=?2 WHERE id=?3",lease,now,row.id).execute(&mut *tx).await.map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(Some((
            NotificationDelivery {
                id: row.id,
                event_id: row.event_id,
                destination: row.destination,
                category: row.category,
                state: "pending".into(),
                attempts: row.attempts + 1,
                created_at_ms: row.created_at_ms,
                next_attempt_ms: lease,
                updated_at_ms: now,
                last_error: row.last_error,
            },
            destination,
        )))
    }
    pub(crate) async fn complete_delivery(
        &self,
        id: &str,
        state: &str,
        error: Option<&str>,
        delay: u64,
    ) -> Result<(), StoreError> {
        let now = now_ms();
        let next =
            now.saturating_add(i64::try_from(delay.saturating_mul(1000)).unwrap_or(i64::MAX));
        sqlx::query!("UPDATE notification_deliveries SET state=?1,last_error=?2,next_attempt_ms=?3,updated_at_ms=?4 WHERE id=?5 AND state='pending'",state,error,next,now,id).execute(&self.pool).await.map_err(StoreError::database)?;
        Ok(())
    }
}
