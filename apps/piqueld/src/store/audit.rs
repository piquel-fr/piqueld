//! The audit trail: one row per audited API request, newest first. Rows copy
//! account and credential details, so they outlive both, and survive
//! application deletion under their own retention.
//!
//! Rows form a hash chain: they have consecutive IDs, and each stores a
//! `link`, the SHA-256 of its predecessor's link and its own ID and fields
//! (see [`AuditRow::link_from`]). The oldest retained row extends the newest
//! pruned one, `audit_chain.pruned_through` with `pruned_link`.
use super::{Attribution, Store, StoreError, now_ms, page_limit};
use piqueld_core::api::Page;
use piqueld_core::audit::{AuditEvent, AuditFilter, AuditLink, AuditOutcome, AuditVerification};
use piqueld_core::auth::HostOperator;
use sqlx::{QueryBuilder, SqliteConnection};

/// Columns read into [`AuditRow`].
const COLUMNS: &str = "id,created_at_ms,action,outcome,status,user_id,username,credential_id,\
    credential_kind,scoped,peer,request_id,application_id,environment_id,permission,link,tailnet,operator_uid";

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
    /// Tailnet identity of the peer (see `TailnetPeer::describe`).
    pub(crate) tailnet: Option<String>,
    pub(crate) request_id: Option<String>,
    /// Application the route names by ID.
    pub(crate) application_id: Option<String>,
    /// Environment the route names by ID; its application is recorded too.
    pub(crate) environment_id: Option<String>,
    pub(crate) permission: Option<&'static str>,
    /// The host operator, when it made the request instead of an account.
    pub(crate) operator: Option<HostOperator>,
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
    tailnet: Option<String>,
    operator_uid: Option<i64>,
}

impl NewAuditEvent {
    /// The row to store as `id` at `created_at_ms`, before its link exists.
    fn row(&self, id: i64, created_at_ms: i64) -> AuditRow {
        AuditRow {
            id,
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
            tailnet: self.tailnet.clone(),
            operator_uid: self.operator.map(|operator| i64::from(operator.uid)),
        }
    }
}

impl AuditRow {
    /// This row's link after `previous`: lowercase hex SHA-256 of a JSON
    /// array holding `previous` and every field, in column order. `tailnet`
    /// and `operator_uid`, added later, are appended only when present, so
    /// links of earlier rows stay valid: `[fields, tailnet]` with a tailnet
    /// identity alone, `[fields, tailnet, operator_uid]` (`tailnet` possibly
    /// null) with an operator.
    fn link_from(&self, previous: &str) -> Result<String, StoreError> {
        use sha2::{Digest, Sha256};
        let fields = (
            previous,
            self.id,
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
        let bytes = match (&self.tailnet, self.operator_uid) {
            (None, None) => serde_json::to_vec(&fields),
            (Some(tailnet), None) => serde_json::to_vec(&(fields, tailnet)),
            (tailnet, Some(operator)) => serde_json::to_vec(&(fields, tailnet, operator)),
        }
        .map_err(StoreError::corrupt)?;
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
            tailnet: self.tailnet,
            request_id: self.request_id,
            application_id: self.application_id,
            environment_id: self.environment_id,
            permission: self.permission,
            operator: self.operator_uid.map(Store::host_operator).transpose()?,
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
        self.record_audit_at(event, now_ms()).await
    }

    /// [`Store::record_audit`] at `created_at_ms`.
    pub(super) async fn record_audit_at(
        &self,
        event: &NewAuditEvent,
        created_at_ms: i64,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let head = Self::audit_head_on(&mut tx).await?;
        // Only a tampered trail reaches the highest possible ID.
        let id = head.id.checked_add(1).ok_or(StoreError::Corrupt)?;
        let mut row = event.row(id, created_at_ms);
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
        row.link = Some(row.link_from(&head.link)?);
        sqlx::query!(
            "INSERT INTO audit_events(created_at_ms,action,outcome,status,user_id,username,credential_id,credential_kind,scoped,peer,request_id,application_id,environment_id,permission,link,id,tailnet,operator_uid) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
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
            row.id,
            row.tailnet,
            row.operator_uid,
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        if event.outcome == AuditOutcome::Denied {
            let actor = Attribution {
                user_id: row.user_id.as_deref(),
                credential_id: row.credential_id.as_deref(),
                operator: event.operator,
            };
            let (peer, username) = (row.peer.as_deref(), row.username.as_deref());
            Self::check_denial_burst_on(&mut tx, peer, actor, username, row.created_at_ms).await?;
        }
        tx.commit().await.map_err(StoreError::database)
    }

    /// The record a new one extends: the newest record, else the anchor,
    /// else the chain's empty start before ID 1.
    /// # Errors
    /// Returns [`StoreError::Corrupt`] for a half-stored anchor.
    async fn audit_head_on(db: &mut SqliteConnection) -> Result<AuditLink, StoreError> {
        let anchor = Self::audit_anchor_on(&mut *db).await?;
        let newest = sqlx::query!(
            r#"SELECT id AS "id!",COALESCE(link,'') AS "link!: String" FROM audit_events
            ORDER BY id DESC LIMIT 1"#
        )
        .fetch_optional(db)
        .await
        .map_err(StoreError::database)?;
        Ok(newest.map_or_else(
            || anchor.unwrap_or_else(Self::audit_start),
            |row| AuditLink {
                id: row.id,
                link: row.link,
            },
        ))
    }

    /// The newest pruned record, which the oldest retained one extends;
    /// `None` until something is pruned.
    /// # Errors
    /// Returns [`StoreError::Corrupt`] when only its ID or its link is stored,
    /// rather than silently verifying from the start.
    async fn audit_anchor_on(db: &mut SqliteConnection) -> Result<Option<AuditLink>, StoreError> {
        let chain = sqlx::query!("SELECT pruned_through,pruned_link FROM audit_chain")
            .fetch_one(db)
            .await
            .map_err(StoreError::database)?;
        match (chain.pruned_through, chain.pruned_link) {
            (Some(id), Some(link)) => Ok(Some(AuditLink { id, link })),
            (None, None) => Ok(None),
            _ => Err(StoreError::Corrupt),
        }
    }

    /// What the first record extends: nothing, before ID 1.
    fn audit_start() -> AuditLink {
        AuditLink {
            id: 0,
            link: String::new(),
        }
    }

    /// Recomputes every retained record's link in order, inside one read
    /// transaction so concurrent pruning cannot interleave.
    /// # Errors
    /// Returns storage errors, or [`StoreError::Corrupt`] for a half-stored
    /// anchor.
    pub async fn verify_audit(&self) -> Result<AuditVerification, StoreError> {
        let mut tx = self.pool.begin().await.map_err(StoreError::database)?;
        Self::verify_audit_on(&mut tx, i64::MAX).await
    }

    /// Checks records from the pruning anchor through ID `through`: each must
    /// have the next ID and the link that ID, its fields, and its predecessor
    /// give.
    async fn verify_audit_on(
        db: &mut SqliteConnection,
        through: i64,
    ) -> Result<AuditVerification, StoreError> {
        let anchor = Self::audit_anchor_on(&mut *db).await?;
        let mut previous = anchor.clone().unwrap_or_else(Self::audit_start);
        let mut result = AuditVerification {
            checked: 0,
            anchor,
            head: None,
            broken_at: None,
        };
        // Every row, including any at the lowest possible ID, in pages that
        // seek to the inclusive cursor `from`.
        let page = format!(
            "SELECT {COLUMNS} FROM audit_events WHERE id >= ?1 AND id <= ?2 ORDER BY id LIMIT 500"
        );
        let mut from = i64::MIN;
        loop {
            let rows = sqlx::query_as::<_, AuditRow>(&page)
                .bind(from)
                .bind(through)
                .fetch_all(&mut *db)
                .await
                .map_err(StoreError::database)?;
            let Some(last) = rows.last() else {
                return Ok(result);
            };
            // The highest possible ID ends the trail.
            let next = last.id.checked_add(1);
            for row in rows {
                let expected = row.link_from(&previous.link)?;
                if previous.id.checked_add(1) != Some(row.id)
                    || row.link.as_ref() != Some(&expected)
                {
                    result.broken_at = Some(row.id);
                    return Ok(result);
                }
                result.checked += 1;
                previous = AuditLink {
                    id: row.id,
                    link: expected,
                };
                result.head = Some(previous.clone());
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
    /// configured retention, keeping it as the chain's new anchor. Zero days
    /// disables pruning. Rows are only pruned while the chain through them
    /// verifies, so pruning never erases evidence of tampering; a broken
    /// chain is logged and left alone, without stopping other pruning.
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
        let newest = sqlx::query_scalar!(
            r#"SELECT id AS "id!" FROM audit_events WHERE created_at_ms < ?1 ORDER BY id DESC LIMIT 1"#,
            cutoff,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let Some(newest) = newest else {
            return Ok(());
        };
        let verified = Self::verify_audit_on(&mut tx, newest).await?;
        let Some(anchor) = verified.head.filter(|_| verified.broken_at.is_none()) else {
            tracing::error!(
                broken_at = verified.broken_at,
                "the audit trail is broken; it is not pruned, to keep the evidence"
            );
            return Ok(());
        };
        sqlx::query!("DELETE FROM audit_events WHERE id <= ?1", anchor.id)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        sqlx::query!(
            "UPDATE audit_chain SET pruned_through=?1,pruned_link=?2",
            anchor.id,
            anchor.link
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }
}
