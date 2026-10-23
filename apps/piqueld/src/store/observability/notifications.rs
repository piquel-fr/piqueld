//! Durable notification conditions and delivery outbox, driven by committed events.
use super::DestinationRow;
use crate::store::{Paging, Store, StoreError, new_id, now_ms, seconds_ms};
use piqueld_core::{
    Event,
    api::Page,
    observability::{
        DeliveryState, Diagnostic, DiagnosticCode, EventScope, NotificationCategory,
        NotificationDelivery, NotificationSubject,
    },
};
use sqlx::{Sqlite, Transaction};

/// How long a dependency or service failure must be observed continuously
/// before it notifies.
const FAILURE_THRESHOLD_MS: i64 = 120_000;
/// How long a delivery is retried automatically.
const RETRY_WINDOW_MS: i64 = 86_400_000;

/// Why a delivery that exists cannot be retried by hand.
#[derive(Debug, thiserror::Error)]
pub enum RetryError {
    /// Only failed deliveries are retried.
    #[error("the delivery is {0}; only failed deliveries can be retried")]
    NotFailed(DeliveryState),
    /// This version does not know the delivery's category.
    #[error("this daemon version does not know the delivery's category, so it cannot be retried")]
    UnknownCategory,
    /// Its destination was deleted, replaced or disabled, or no longer
    /// receives its category, application or previews.
    #[error(
        "the delivery's destination was deleted, replaced or disabled, or no longer receives it, so it cannot be retried"
    )]
    NotReceived,
    /// Its destination acknowledged the recovery that followed this failure.
    #[error(
        "the delivery's destination already acknowledged the recovery that followed it, so it cannot be retried"
    )]
    Recovered,
    /// A recovery whose incident has opened again would be stale.
    #[error("the recovery's incident has opened again, so it cannot be retried")]
    Reopened,
    /// A closed incident is not announced again without its recovery.
    #[error("the alert's incident has closed, so it cannot be retried")]
    Closed,
}

impl Store {
    /// Reads delivery history without destination credentials or response
    /// bodies, leaving out deliveries of a category this version does not know.
    /// # Errors
    /// Returns pagination or storage errors.
    pub async fn deliveries(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<NotificationDelivery>, StoreError> {
        let paging = Paging::new(limit)?;
        let cursor = Paging::cursor(cursor)?;
        let mut rows = sqlx::query!(
            "SELECT id AS \"id!\",event_id,destination,subject_json,category,state,attempts,created_at_ms,next_attempt_ms,updated_at_ms,last_error
            FROM notification_deliveries
            WHERE ?1 IS NULL OR id<?1
            ORDER BY id DESC LIMIT ?2",
            cursor,
            paging.fetch,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?;
        let next_cursor = paging.next_cursor(&mut rows, |r| r.id.clone());
        Ok(Page {
            items: rows
                .into_iter()
                .filter_map(|r| {
                    // Deliveries of a category this version does not know,
                    // such as one added by a newer version, are left out.
                    let Some(category) = NotificationCategory::parse(&r.category) else {
                        tracing::warn!(delivery_id = %r.id, category = %r.category, "delivery of an unknown category is not listed");
                        return None;
                    };
                    Some(Ok(NotificationDelivery {
                        id: r.id,
                        event_id: r.event_id,
                        destination: r.destination,
                        subject: match Self::subject(r.subject_json) {
                            Ok(subject) => subject,
                            Err(error) => return Some(Err(error)),
                        },
                        category,
                        state: match DeliveryState::parse(&r.state) {
                            Some(state) => state,
                            None => return Some(Err(StoreError::Corrupt)),
                        },
                        attempts: r.attempts,
                        created_at_ms: r.created_at_ms,
                        next_attempt_ms: r.next_attempt_ms,
                        updated_at_ms: r.updated_at_ms,
                        last_error: r.last_error,
                    }))
                })
                .collect::<Result<_, StoreError>>()?,
            next_cursor,
        })
    }
    /// Decodes a delivery's stored subject; deliveries queued before
    /// subjects were stored have an empty one.
    fn subject(json: Option<String>) -> Result<NotificationSubject, StoreError> {
        json.map(|json| serde_json::from_str(&json))
            .transpose()
            .map(Option::unwrap_or_default)
            .map_err(StoreError::corrupt)
    }
    /// Retries a failed delivery only while its destination still exists,
    /// unchanged, enabled, receiving its category and, unless it is a
    /// recovery, the event.
    /// Also refuses failures already followed by a delivered recovery,
    /// recoveries whose incident has reopened, and failures whose incident has
    /// since closed. Security events and daemon starts open no incident, so
    /// their deliveries stay retryable. Resets the delivery to `pending` with a
    /// fresh retry window. `actor` needs `system:operate`, checked in the same
    /// transaction.
    ///
    /// # Errors
    /// Returns absence, a [`RetryError`] naming why it cannot be retried, or
    /// storage errors.
    pub async fn retry_delivery(
        &self,
        actor: crate::store::Actor<'_>,
        id: &str,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let operate = piqueld_core::access::Permission::Global(
            piqueld_core::access::GlobalPermission::SystemOperate,
        );
        actor.require_on(&mut tx, operate, None).await?;
        let row = sqlx::query!(
            "SELECT d.state,d.category,d.event_id,c.category IS NOT NULL
                AND (d.category='recovery' OR EXISTS (SELECT 1 FROM notification_audience au
                    WHERE au.destination_id=n.id AND au.event_id=d.event_id)) AS \"delivers!: bool\"
            FROM notification_deliveries d
            LEFT JOIN notification_destinations n ON n.id=d.destination_id AND n.enabled=1
            LEFT JOIN notification_destination_categories c ON c.destination_id=n.id AND c.category=d.category
            WHERE d.id=?1",
            id,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        let state = DeliveryState::parse(&row.state).ok_or(StoreError::Corrupt)?;
        if state != DeliveryState::Failed {
            return Err(RetryError::NotFailed(state).into());
        }
        // A category this version does not know cannot be delivered.
        let category =
            NotificationCategory::parse(&row.category).ok_or(RetryError::UnknownCategory)?;
        if !row.delivers {
            return Err(RetryError::NotReceived.into());
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
            return Err(RetryError::Recovered.into());
        }
        match category {
            // A recovery is stale once its incident has opened again.
            NotificationCategory::Recovery => {
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
                    return Err(RetryError::Reopened.into());
                }
            }
            // Security events, daemon starts and deployments report one
            // occurrence, not an incident that closes, so they stay retryable.
            NotificationCategory::Security
            | NotificationCategory::DaemonStarted
            | NotificationCategory::DeploymentSucceeded => {}
            // A closed incident cannot be announced again without a matching recovery.
            _ => {
                let open = sqlx::query_scalar!(
                    "SELECT COUNT(*) FROM notification_conditions WHERE event_id=?1",
                    row.event_id,
                )
                .fetch_one(&mut *tx)
                .await
                .map_err(StoreError::database)?;
                if open == 0 {
                    return Err(RetryError::Closed.into());
                }
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
    /// Restarts the sustained-failure window of conditions not yet notified,
    /// at startup: process downtime does not count as continuous observation.
    pub(crate) async fn restart_failure_windows(&self) -> Result<(), StoreError> {
        let now = now_ms();
        self.write(sqlx::query!(
            "UPDATE notification_conditions SET first_seen_ms=?1,last_seen_ms=?1 WHERE notified=0",
            now,
        ))
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }
    /// Advances the notification cursor over the next batch (up to a page) of
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
            .events(
                None,
                Some(&format!("v1:{cursor}")),
                crate::store::MAX_PAGE_SIZE,
            )
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
    /// recoveries), and notifies unless it was a deletion. Failed operations and daemon diagnostics open a condition and
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
                    &format!("{}:{category}", app.as_deref().unwrap_or("daemon")),
                    event.id,
                )
                .await?;
            }
            let operation = event.operation_id.as_deref();
            let kind = sqlx::query_scalar!("SELECT kind FROM operations WHERE id=?1", operation)
                .fetch_optional(&mut **tx)
                .await
                .map_err(StoreError::database)?;
            if app.is_some() && kind.is_some_and(|kind| kind != "delete") {
                let deployed = NotificationCategory::DeploymentSucceeded;
                return Self::enqueue(tx, event.id, deployed).await;
            }
        }
        // Security events and daemon starts each notify; neither is a
        // condition that recovers.
        if crate::store::SecurityEvent::parse(&event.kind).is_some() {
            let security = NotificationCategory::Security;
            return Self::enqueue(tx, event.id, security).await;
        }
        if event.kind == "daemon_started" {
            let started = NotificationCategory::DaemonStarted;
            return Self::enqueue(tx, event.id, started).await;
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
            Self::enqueue(tx, event.id, category).await?;
        }
        Ok(())
    }
    /// Queues a delivery of `event` to every enabled destination in its
    /// `notification_audience` receiving `category` since before it.
    async fn enqueue(
        tx: &mut Transaction<'_, Sqlite>,
        event: i64,
        category: NotificationCategory,
    ) -> Result<(), StoreError> {
        let category = category.as_str();
        let destinations = sqlx::query!(
            "SELECT d.id AS \"id!\",d.name FROM notification_audience au
            JOIN notification_destination_categories c ON c.destination_id=au.destination_id
                AND c.category=?2 AND c.after_event_id<au.event_id
            JOIN notification_destinations d ON d.id=au.destination_id AND d.enabled=1
            WHERE au.event_id=?1",
            event,
            category,
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        for destination in destinations {
            Self::enqueue_destination(tx, event, category, &destination.id, &destination.name)
                .await?;
        }
        Ok(())
    }

    /// Queues one delivery of `event` to a destination idempotently and
    /// returns its ID, new or existing. The delivery keeps the current names
    /// of the event's application and environment as its subject.
    async fn enqueue_destination(
        tx: &mut Transaction<'_, Sqlite>,
        event: i64,
        category: &str,
        destination: &str,
        name: &str,
    ) -> Result<String, StoreError> {
        let now = now_ms();
        let id = new_id("delivery");
        sqlx::query!(
            "INSERT INTO notification_deliveries(
            id,event_id,destination,destination_id,category,state,
            created_at_ms,retry_started_at_ms,next_attempt_ms,updated_at_ms,subject_json
            ) SELECT ?1,?2,?3,?4,?5,'pending',?6,?6,?6,?6,
            json_object('application',a.name,'environment',v.name,
                'preview',json(IIF(v.kind='preview','true','false')),'branch',v.branch)
            FROM events e LEFT JOIN applications a ON a.id=e.application_id
            LEFT JOIN environments v ON v.id=e.environment_id
            WHERE e.id=?2
            ON CONFLICT(event_id,destination,category) DO NOTHING",
            id,
            event,
            name,
            destination,
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
            name,
            category,
        )
        .fetch_one(&mut **tx)
        .await
        .map_err(StoreError::database)
    }
    /// Clears an open condition. If it was notified, queues a recovery for
    /// each destination that received (or is still receiving) the failure and
    /// still receives both its category and recoveries, linking them in
    /// `notification_recovery_sources` so the recovery is sent only after the
    /// failure is acknowledged.
    async fn close_condition(
        tx: &mut Transaction<'_, Sqlite>,
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
        if let Some(condition) = condition.filter(|condition| condition.notified != 0) {
            let recovery = NotificationCategory::Recovery.as_str();
            let failures = sqlx::query!(
                "SELECT f.id AS \"id!\",d.id AS \"destination_id!\",d.name
                FROM notification_deliveries f
                JOIN notification_destinations d ON d.id=f.destination_id AND d.enabled=1
                JOIN notification_destination_categories c ON c.destination_id=d.id AND c.category=f.category
                JOIN notification_destination_categories r ON r.destination_id=d.id AND r.category=?3
                    AND r.after_event_id<?4
                WHERE f.event_id=?1 AND f.category=?2 AND f.state IN ('pending','delivered')",
                condition.event_id,
                condition.category,
                recovery,
                event,
            )
            .fetch_all(&mut **tx)
            .await
            .map_err(StoreError::database)?;
            for failure in failures {
                let recovery = Self::enqueue_destination(
                    tx,
                    event,
                    recovery,
                    &failure.destination_id,
                    &failure.name,
                )
                .await?;
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
        // An environment's keys are its ID, or `<code>:<ID>` for a
        // dependency of the daemon it hosts, like a telemetry receiver.
        let code = match (application, key.split_once(':')) {
            (Some(_), Some((code, _))) => {
                DiagnosticCode::parse(code).ok_or(StoreError::InvalidInput)?
            }
            (Some(_), None) => DiagnosticCode::ServiceDegraded,
            (None, _) => DiagnosticCode::parse(key).ok_or(StoreError::InvalidInput)?,
        };
        // Environments are named as `application / environment`.
        let environment = match application {
            Some(id) => sqlx::query_scalar!(
                "SELECT a.name||' / '||v.name AS \"name!: String\" FROM environments v
                JOIN applications a ON a.id=v.application_id WHERE v.id=?1",
                id,
            )
            .fetch_optional(&mut **tx)
            .await
            .map_err(StoreError::database)?
            .or_else(|| Some(id.to_owned())),
            None => None,
        };
        let subject = match (environment, key) {
            (Some(name), key) if key.starts_with("telemetry_export_failed:") => {
                format!("Telemetry export to {name}")
            }
            (Some(name), _) => format!("Service health of {name}"),
            (None, "docker_unavailable") => "Docker Engine".into(),
            (None, "swarm_manager_unavailable") => "Swarm manager".into(),
            (None, "ingress_unavailable") => "Managed ingress".into(),
            (None, "certificate_renewal_failed") => "DNS-01 certificate renewal".into(),
            (None, "telemetry_export_failed") => "Telemetry export".into(),
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
                Self::close_condition(&mut tx, &condition_key, event).await?;
            }
        }
        if failed {
            Self::notify_sustained_failure(&mut tx, &condition_key, category, gap, observed)
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
    /// continuously for [`FAILURE_THRESHOLD_MS`].
    async fn notify_sustained_failure(
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
        if let Some(condition) = sqlx::query!(
            "SELECT event_id FROM notification_conditions WHERE key=?1 AND notified=0 AND last_seen_ms-first_seen_ms>=?2",
            condition_key,
            FAILURE_THRESHOLD_MS,
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)? {
            Self::enqueue(tx, condition.event_id, category).await?;
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
    /// 2. cancels recoveries none of whose failures can still be delivered,
    ///    and deployments whose recovery was delivered;
    /// 3. picks the earliest due delivery, holding recoveries until every linked
    ///    failure is settled and at least one was delivered, and deployments
    ///    while their recovery is pending;
    /// 4. leases it for 30 seconds by pushing `next_attempt_ms` and counting the attempt.
    ///
    /// A delivery that can no longer be sent (an unknown category, or a
    /// destination or category since removed, whose changes cancel pending
    /// deliveries anyway) is settled with an error instead, so it cannot
    /// block the ones behind it.
    pub(crate) async fn claim_delivery(
        &self,
    ) -> Result<Option<(NotificationDelivery, DestinationRow)>, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let now = now_ms();
        Self::settle_expired_deliveries(&mut tx, now).await?;
        let (row, category, destination) = loop {
            let row = sqlx::query!(
                "SELECT d.id AS \"id!\",d.destination,d.subject_json,d.event_id,d.category,d.attempts,
                d.created_at_ms,d.updated_at_ms,d.last_error,
                n.id AS \"destination_id?\",n.kind AS \"kind?\",n.nonce AS \"nonce?\",
                n.ciphertext AS \"ciphertext?\",c.message,
                c.category IS NOT NULL AS \"receives!: bool\"
                FROM notification_deliveries d
                LEFT JOIN notification_destinations n ON n.id=d.destination_id AND n.enabled=1
                LEFT JOIN notification_destination_categories c ON c.destination_id=n.id
                    AND c.category=d.category
                WHERE d.state='pending' AND d.next_attempt_ms<=?1
                AND (d.category!='recovery' OR (
                EXISTS (SELECT 1 FROM notification_recovery_sources s
                JOIN notification_deliveries f ON f.id=s.failure_id
                WHERE s.recovery_id=d.id AND f.state='delivered')
                AND NOT EXISTS (SELECT 1 FROM notification_recovery_sources s
                JOIN notification_deliveries f ON f.id=s.failure_id
                WHERE s.recovery_id=d.id AND f.state='pending')
                ))
                AND (d.category!='deployment_succeeded' OR NOT EXISTS (
                SELECT 1 FROM notification_deliveries r WHERE r.event_id=d.event_id
                AND r.category='recovery' AND r.destination_id=d.destination_id
                AND r.state='pending'))
                ORDER BY d.next_attempt_ms,d.id LIMIT 1",
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
            let Some(category) = NotificationCategory::parse(&row.category) else {
                Self::settle_undeliverable(
                    &mut tx,
                    &row.id,
                    DeliveryState::Failed,
                    "Unknown notification category",
                    now,
                )
                .await?;
                continue;
            };
            // A destination detached, deleted, disabled or no longer
            // receiving the category cannot be sent to.
            let (Some(id), Some(kind), Some(nonce), Some(ciphertext), true) = (
                row.destination_id.clone(),
                row.kind.as_deref(),
                row.nonce.clone(),
                row.ciphertext.clone(),
                row.receives,
            ) else {
                Self::settle_undeliverable(
                    &mut tx,
                    &row.id,
                    DeliveryState::Cancelled,
                    super::DESTINATION_CHANGED,
                    now,
                )
                .await?;
                continue;
            };
            let destination =
                DestinationRow::new(id, kind, row.message.clone(), nonce, ciphertext)?;
            break (row, category, destination);
        };
        let lease = now.saturating_add(30_000);
        sqlx::query!(
            "UPDATE notification_deliveries
            SET retry_started_at_ms=CASE WHEN category IN ('recovery','deployment_succeeded') AND attempts=0 THEN ?2 ELSE retry_started_at_ms END,
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
                subject: Self::subject(row.subject_json)?,
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

    /// Steps 1 and 2 of `claim_delivery`: settles the pending deliveries that
    /// can no longer be sent at `now`.
    async fn settle_expired_deliveries(
        tx: &mut Transaction<'_, Sqlite>,
        now: i64,
    ) -> Result<(), StoreError> {
        let expired = now.saturating_sub(RETRY_WINDOW_MS);
        // A recovery's retry window starts when it can first be sent, not while
        // it is waiting for acknowledgement of the original failure; so does
        // a deployment's, waiting for the recovery it brought.
        sqlx::query!(
            "UPDATE notification_deliveries
            SET state='failed',last_error='Delivery retry window expired',updated_at_ms=?1
            WHERE state='pending' AND retry_started_at_ms<?2
            AND (category NOT IN ('recovery','deployment_succeeded') OR attempts>0)",
            now,
            expired,
        )
        .execute(&mut **tx)
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
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        // A deployment that cleared a failure is announced by its recovery
        // when that is delivered, and on its own otherwise.
        sqlx::query!(
            "UPDATE notification_deliveries
            SET state='cancelled',last_error='Its recovery was delivered instead',updated_at_ms=?1
            WHERE state='pending' AND category='deployment_succeeded' AND EXISTS (
            SELECT 1 FROM notification_deliveries r WHERE r.event_id=notification_deliveries.event_id
            AND r.category='recovery' AND r.destination_id=notification_deliveries.destination_id
            AND r.state='delivered')",
            now,
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// Settles the pending delivery `id` as `state` with `error`, for
    /// `claim_delivery` to move past it.
    async fn settle_undeliverable(
        tx: &mut Transaction<'_, Sqlite>,
        id: &str,
        state: DeliveryState,
        error: &str,
        now: i64,
    ) -> Result<(), StoreError> {
        let state = state.as_str();
        sqlx::query!(
            "UPDATE notification_deliveries SET state=?1,last_error=?2,updated_at_ms=?3 WHERE id=?4",
            state,
            error,
            now,
            id,
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Ok(())
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
        let now = now_ms();
        let next = now.saturating_add(seconds_ms(delay));
        let state = state.as_str();
        self.write(sqlx::query!(
            "UPDATE notification_deliveries
            SET state=?1,last_error=?2,next_attempt_ms=?3,updated_at_ms=?4
            WHERE id=?5 AND state='pending'",
            state,
            error,
            next,
            now,
            id,
        ))
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }
}
