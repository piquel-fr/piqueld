//! The audit trail: one row per audited API request, newest first. Rows copy
//! account and credential details, so they outlive both, and survive
//! application deletion under their own retention.
//!
//! Rows form a hash chain: each stores a `link`, the SHA-256 of its
//! predecessor's link and its own fields (see [`AuditRow::link_from`]). The
//! oldest retained row extends `audit_chain.pruned_link`, the link of the
//! newest pruned row. Rows from before the chain existed have no link; they
//! end at `audit_chain.unlinked_through`.
use super::{Attribution, Store, StoreError, now_ms, page_limit};
use piqueld_core::api::Page;
use piqueld_core::audit::{AuditEvent, AuditFilter, AuditOutcome, AuditVerification};
use sqlx::{QueryBuilder, SqliteConnection};

/// Columns read into [`AuditRow`].
const COLUMNS: &str = "id,created_at_ms,action,outcome,status,user_id,username,credential_id,\
    credential_kind,scoped,peer,request_id,application_id,environment_id,permission,link";

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
    link: Option<String>,
}

impl NewAuditEvent {
    /// The row to store at `created_at_ms`, before its ID and link exist.
    fn row(&self, created_at_ms: i64) -> AuditRow {
        AuditRow {
            id: 0,
            created_at_ms,
            action: self.action.clone(),
            outcome: self.outcome.as_str().to_owned(),
            status: i64::from(self.status),
            user_id: self.user_id.clone(),
            username: self.username.clone(),
            credential_id: self.credential_id.clone(),
            credential_kind: self.credential_kind.map(str::to_owned),
            scoped: self.scoped,
            peer: self.peer.clone(),
            request_id: self.request_id.clone(),
            application_id: self.application_id.clone(),
            environment_id: self.environment_id.clone(),
            permission: self.permission.map(str::to_owned),
            link: None,
        }
    }
}

impl AuditRow {
    /// This row's link after `previous`: lowercase hex SHA-256 of a JSON
    /// array holding `previous` and every field except the ID, in column
    /// order. The ID is left out because rows are linked by order instead.
    fn link_from(&self, previous: &str) -> Result<String, StoreError> {
        use sha2::{Digest, Sha256};
        let fields = (
            previous,
            self.created_at_ms,
            &self.action,
            &self.outcome,
            self.status,
            &self.user_id,
            &self.username,
            &self.credential_id,
            &self.credential_kind,
            self.scoped,
            &self.peer,
            &self.request_id,
            &self.application_id,
            &self.environment_id,
            &self.permission,
        );
        let bytes = serde_json::to_vec(&fields).map_err(StoreError::corrupt)?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }

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
    /// Records one audited request, linked to the newest record, and raises
    /// a security event when it completes a burst of refusals. A request about
    /// an environment also records the environment's application, while it
    /// exists, if the caller may know it: when the request was allowed, or
    /// refused for a missing permission, which only happens on applications
    /// the caller can read. Callers read their own trail and must not learn
    /// which application owns an environment hidden from them.
    /// # Errors
    /// Returns storage errors.
    pub(crate) async fn record_audit(&self, event: &NewAuditEvent) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let mut row = event.row(now_ms());
        if row.application_id.is_none()
            && (row.outcome == AuditOutcome::Allowed.as_str() || row.permission.is_some())
            && let Some(environment) = &row.environment_id
        {
            row.application_id = sqlx::query_scalar!(
                r#"SELECT application_id AS "application_id!" FROM environments WHERE id=?1"#,
                environment
            )
            .fetch_optional(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        }
        let previous = Self::audit_head_on(&mut tx).await?;
        row.link = Some(row.link_from(&previous)?);
        sqlx::query!(
            "INSERT INTO audit_events(created_at_ms,action,outcome,status,user_id,username,credential_id,credential_kind,scoped,peer,request_id,application_id,environment_id,permission,link) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            row.created_at_ms,
            row.action,
            row.outcome,
            row.status,
            row.user_id,
            row.username,
            row.credential_id,
            row.credential_kind,
            row.scoped,
            row.peer,
            row.request_id,
            row.application_id,
            row.environment_id,
            row.permission,
            row.link,
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        if event.outcome == AuditOutcome::Denied {
            let actor = Attribution {
                user_id: row.user_id.as_deref(),
                credential_id: row.credential_id.as_deref(),
            };
            let (peer, username) = (row.peer.as_deref(), row.username.as_deref());
            Self::check_denial_burst_on(&mut tx, peer, actor, username, row.created_at_ms).await?;
        }
        tx.commit().await.map_err(StoreError::database)
    }

    /// The link a new record extends: the newest record's (empty if it
    /// predates the chain), else the newest pruned record's, else empty.
    async fn audit_head_on(db: &mut SqliteConnection) -> Result<String, StoreError> {
        sqlx::query_scalar!(
            r#"SELECT COALESCE(
                (SELECT COALESCE(link,'') FROM audit_events ORDER BY id DESC LIMIT 1),
                (SELECT pruned_link FROM audit_chain),
                ''
            ) AS "head!: String""#
        )
        .fetch_one(db)
        .await
        .map_err(StoreError::database)
    }

    /// Recomputes every retained record's link in order, inside one read
    /// transaction so concurrent pruning cannot interleave.
    ///
    /// Only records from before the chain may lack a link, and only before
    /// any linked record (or linked pruning anchor); any other missing link
    /// breaks the chain.
    /// # Errors
    /// Returns storage errors.
    pub async fn verify_audit(&self) -> Result<AuditVerification, StoreError> {
        let mut tx = self.pool.begin().await.map_err(StoreError::database)?;
        let chain = sqlx::query!("SELECT pruned_link,unlinked_through FROM audit_chain")
            .fetch_one(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        let mut previous = chain.pruned_link.unwrap_or_default();
        let legacy = |id: i64| chain.unlinked_through.is_some_and(|last| id <= last);
        let mut result = AuditVerification {
            checked: 0,
            unlinked: 0,
            head: None,
            broken_at: None,
        };
        // Every row, including any written with a zero or negative ID, in
        // pages that seek to the inclusive cursor `from`.
        let page =
            format!("SELECT {COLUMNS} FROM audit_events WHERE id >= ?1 ORDER BY id LIMIT 500");
        let mut from = i64::MIN;
        loop {
            let rows = sqlx::query_as::<_, AuditRow>(&page)
                .bind(from)
                .fetch_all(&mut *tx)
                .await
                .map_err(StoreError::database)?;
            let Some(last) = rows.last() else {
                return Ok(result);
            };
            // The newest possible ID ends the trail.
            let next = last.id.checked_add(1);
            for row in rows {
                match &row.link {
                    None if previous.is_empty() && legacy(row.id) => result.unlinked += 1,
                    Some(link) if *link == row.link_from(&previous)? => {
                        result.checked += 1;
                        previous.clone_from(link);
                        result.head = Some(previous.clone());
                    }
                    _ => {
                        result.broken_at = Some(row.id);
                        return Ok(result);
                    }
                }
            }
            let Some(next) = next else {
                return Ok(result);
            };
            from = next;
        }
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
        let mut query =
            QueryBuilder::new(format!("SELECT {COLUMNS} FROM audit_events WHERE id < "));
        query.push_bind(before);
        // Usernames compare case-insensitively, like accounts' own.
        for (column, value, collation) in [
            ("user_id", filter.user_id.as_deref(), ""),
            ("username", filter.username.as_deref(), " COLLATE NOCASE"),
            ("credential_id", filter.credential_id.as_deref(), ""),
            ("outcome", filter.outcome.map(AuditOutcome::as_str), ""),
        ] {
            if let Some(value) = value {
                query
                    .push(" AND ")
                    .push(column)
                    .push(" = ")
                    .push_bind(value)
                    .push(collation);
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

    /// Prunes the oldest audit rows, through the newest one past the
    /// configured retention, keeping its link as the chain's new anchor. Zero
    /// days disables pruning.
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
        let newest = sqlx::query!(
            "SELECT id,link FROM audit_events WHERE created_at_ms < ?1 ORDER BY id DESC LIMIT 1",
            cutoff,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let Some(newest) = newest else {
            return Ok(());
        };
        sqlx::query!("DELETE FROM audit_events WHERE id <= ?1", newest.id)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        sqlx::query!("UPDATE audit_chain SET pruned_link=?1", newest.link)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }
}
