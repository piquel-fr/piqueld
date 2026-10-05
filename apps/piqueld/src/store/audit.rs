//! The audit trail: one row per audited API request, newest first. Rows copy
//! account and credential details, so they outlive both, and survive
//! application deletion under their own retention.
use super::{Store, StoreError, now_ms, page_limit};
use piqueld_core::api::Page;
use piqueld_core::audit::{AuditEvent, AuditFilter, AuditOutcome};
use sqlx::QueryBuilder;

/// An audited request to record.
pub(crate) struct NewAuditEvent {
    /// Method and route template.
    pub(crate) action: String,
    pub(crate) outcome: AuditOutcome,
    pub(crate) status: u16,
    pub(crate) user_id: Option<String>,
    pub(crate) username: Option<String>,
    pub(crate) credential_id: Option<String>,
    pub(crate) credential_kind: Option<&'static str>,
    pub(crate) scoped: Option<bool>,
    pub(crate) peer: Option<String>,
    pub(crate) request_id: Option<String>,
    /// Application the route names by ID.
    pub(crate) application_id: Option<String>,
    /// Environment the route names by ID; its application is recorded too.
    pub(crate) environment_id: Option<String>,
    pub(crate) permission: Option<&'static str>,
}

/// Raw `audit_events` row.
#[derive(sqlx::FromRow)]
struct AuditRow {
    id: i64,
    created_at_ms: i64,
    action: String,
    outcome: String,
    status: i64,
    user_id: Option<String>,
    username: Option<String>,
    credential_id: Option<String>,
    credential_kind: Option<String>,
    scoped: Option<bool>,
    peer: Option<String>,
    request_id: Option<String>,
    application_id: Option<String>,
    environment_id: Option<String>,
    permission: Option<String>,
}

impl AuditRow {
    fn into_event(self) -> Result<AuditEvent, StoreError> {
        Ok(AuditEvent {
            id: self.id,
            created_at_ms: self.created_at_ms,
            action: self.action,
            outcome: AuditOutcome::parse(&self.outcome).ok_or(StoreError::Corrupt)?,
            status: u16::try_from(self.status).map_err(StoreError::corrupt)?,
            user_id: self.user_id,
            username: self.username,
            credential_id: self.credential_id,
            credential_kind: self.credential_kind,
            scoped: self.scoped,
            peer: self.peer,
            request_id: self.request_id,
            application_id: self.application_id,
            environment_id: self.environment_id,
            permission: self.permission,
        })
    }
}

impl Store {
    /// Records one audited request. A request about an environment also
    /// records the environment's application, while it exists.
    /// # Errors
    /// Returns storage errors.
    pub(crate) async fn record_audit(&self, event: &NewAuditEvent) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let now = now_ms();
        let outcome = event.outcome.as_str();
        sqlx::query!(
            "INSERT INTO audit_events(created_at_ms,action,outcome,status,user_id,username,credential_id,credential_kind,scoped,peer,request_id,application_id,environment_id,permission) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,COALESCE(?12,(SELECT application_id FROM environments WHERE id=?13)),?13,?14)",
            now,
            event.action,
            outcome,
            event.status,
            event.user_id,
            event.username,
            event.credential_id,
            event.credential_kind,
            event.scoped,
            event.peer,
            event.request_id,
            event.application_id,
            event.environment_id,
            event.permission
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }

    /// Reads audited requests matching `filter`, newest first, before the
    /// opaque `v1:<id>` cursor.
    /// # Errors
    /// Returns invalid cursor or storage errors.
    pub async fn audit_events(
        &self,
        filter: &AuditFilter,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<AuditEvent>, StoreError> {
        let fetch = page_limit(limit)? + 1;
        let before = cursor
            .map(Self::event_cursor)
            .transpose()?
            .unwrap_or(i64::MAX);
        let mut query = QueryBuilder::new(
            "SELECT id,created_at_ms,action,outcome,status,user_id,username,credential_id,credential_kind,scoped,peer,request_id,application_id,environment_id,permission FROM audit_events WHERE id < ",
        );
        query.push_bind(before);
        for (column, value) in [
            ("user_id", filter.user_id.as_deref()),
            ("credential_id", filter.credential_id.as_deref()),
            ("outcome", filter.outcome.map(AuditOutcome::as_str)),
        ] {
            if let Some(value) = value {
                query
                    .push(" AND ")
                    .push(column)
                    .push(" = ")
                    .push_bind(value);
            }
        }
        query.push(" ORDER BY id DESC LIMIT ").push_bind(fetch);
        let mut rows = query
            .build_query_as::<AuditRow>()
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::database)?;
        let more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = more
            .then(|| rows.last().map(|row| format!("v1:{}", row.id)))
            .flatten();
        let items = rows
            .into_iter()
            .map(AuditRow::into_event)
            .collect::<Result<_, _>>()?;
        Ok(Page { items, next_cursor })
    }

    /// Prunes audit rows past the configured retention; zero days disables
    /// pruning.
    /// # Errors
    /// Returns storage errors.
    pub(crate) async fn prune_audit(&self) -> Result<(), StoreError> {
        if self.audit_days == 0 {
            return Ok(());
        }
        let cutoff = now_ms().saturating_sub(
            i64::try_from(self.audit_days.saturating_mul(86_400_000)).unwrap_or(i64::MAX),
        );
        let (_writer, mut tx) = self.begin_immediate().await?;
        sqlx::query!("DELETE FROM audit_events WHERE created_at_ms < ?1", cutoff)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }
}
