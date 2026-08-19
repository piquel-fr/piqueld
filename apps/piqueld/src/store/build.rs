//! Build repository implementation.

use super::{
    ApplicationId, ApplicationRow, Build, BuildArtifact, BuildArtifactRepository, BuildLog,
    BuildRepository, SqliteStore, StoreError, WorkState, async_trait, new_id, now_ms,
    valid_bounded_text, valid_error, valid_logical_name, valid_sha256,
};

#[derive(Debug)]
struct BuildRow {
    id: String,
    operation_id: String,
    application_id: String,
    service_name: String,
    state: String,
    source_commit: Option<String>,
    image_reference: Option<String>,
    image_digest: Option<String>,
    error_code: Option<String>,
    error_message: Option<String>,
    created_at_ms: i64,
    updated_at_ms: i64,
    started_at_ms: Option<i64>,
    finished_at_ms: Option<i64>,
}

fn parse_build_row(row: BuildRow) -> Result<Build, StoreError> {
    Ok(Build {
        id: row.id,
        operation_id: row.operation_id,
        application_id: ApplicationId::parse(row.application_id)
            .map_err(|_| StoreError::Corrupt)?,
        service_name: row.service_name,
        state: WorkState::parse(&row.state)?,
        source_commit: row.source_commit,
        image_reference: row.image_reference,
        image_digest: row.image_digest,
        error_code: row.error_code,
        error_message: row.error_message,
        created_at_ms: row.created_at_ms,
        updated_at_ms: row.updated_at_ms,
        started_at_ms: row.started_at_ms,
        finished_at_ms: row.finished_at_ms,
    })
}

impl SqliteStore {
    /// Atomically journals a source build prepared and registry-verified before
    /// the operation begins executing.
    ///
    /// # Errors
    /// Returns [`StoreError`] when build metadata is invalid, the operation is
    /// missing, or the transaction cannot be committed.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_prepared_build(
        &self,
        operation_id: &str,
        application_id: &ApplicationId,
        service_name: &str,
        source_commit: &str,
        image_reference: &str,
        image_digest: &str,
        build_key: &str,
        context_hash: &str,
        logs: &[crate::build::BuildLogEntry],
    ) -> Result<Build, StoreError> {
        if !valid_logical_name(service_name)
            || !valid_sha256(build_key)
            || !valid_sha256(context_hash)
            || source_commit.len() != 40
            || !source_commit.bytes().all(|byte| byte.is_ascii_hexdigit())
            || image_reference.len() > 512
            || image_digest.len() > 512
            || logs.len() > 100_000
        {
            return Err(StoreError::InvalidInput);
        }
        let mut tx = self.begin_immediate().await?;
        let application_id_value = application_id.as_str();
        let operation_exists = sqlx::query_scalar!(
            r#"SELECT 1 AS "present!: i64" FROM operations WHERE id=?1 AND application_id=?2"#,
            operation_id,
            application_id_value
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| StoreError::Database)?
        .is_some();
        if !operation_exists {
            return Err(StoreError::NotFound);
        }
        let id = new_id("build");
        let now = now_ms();
        sqlx::query!(
            "INSERT INTO builds(id,operation_id,application_id,service_name,state,source_commit,image_reference,image_digest,created_at_ms,updated_at_ms,started_at_ms,finished_at_ms) VALUES(?1,?2,?3,?4,'succeeded',?5,?6,?7,?8,?8,?8,?8)",
            id,
            operation_id,
            application_id_value,
            service_name,
            source_commit,
            image_reference,
            image_digest,
            now
        )
        .execute(&mut *tx)
        .await
        .map_err(|_| StoreError::Database)?;
        sqlx::query!(
            "INSERT INTO build_artifacts(build_id,build_key,context_hash,verified,verified_at_ms) VALUES(?1,?2,?3,1,?4)",
            id,
            build_key,
            context_hash,
            now
        )
        .execute(&mut *tx)
        .await
        .map_err(|_| StoreError::Database)?;
        let mut retained = 0_usize;
        for log in logs {
            if log.sequence == 0
                || log.timestamp_ms <= 0
                || !valid_bounded_text(&log.message, 16_384)
            {
                return Err(StoreError::InvalidInput);
            }
            if retained.saturating_add(log.message.len()) > 4 * 1024 * 1024 {
                break;
            }
            retained += log.message.len();
            let sequence = i64::try_from(log.sequence).map_err(|_| StoreError::InvalidInput)?;
            sqlx::query!(
                "INSERT INTO build_logs(build_id,sequence,timestamp_ms,message) VALUES(?1,?2,?3,?4)",
                id,
                sequence,
                log.timestamp_ms,
                log.message
            )
            .execute(&mut *tx)
            .await
            .map_err(|_| StoreError::Database)?;
        }
        tx.commit().await.map_err(|_| StoreError::Database)?;
        Ok(Build {
            id,
            operation_id: operation_id.into(),
            application_id: application_id.clone(),
            service_name: service_name.into(),
            state: WorkState::Succeeded,
            source_commit: Some(source_commit.into()),
            image_reference: Some(image_reference.into()),
            image_digest: Some(image_digest.into()),
            error_code: None,
            error_message: None,
            created_at_ms: now,
            updated_at_ms: now,
            started_at_ms: Some(now),
            finished_at_ms: Some(now),
        })
    }
}

#[async_trait]
impl BuildRepository for SqliteStore {
    async fn create_build(
        &self,
        operation_id: &str,
        application_id: &ApplicationId,
        service_name: &str,
    ) -> Result<Build, StoreError> {
        let mut tx = self.begin_immediate().await?;
        let application_id_value = application_id.as_str();
        let operation = sqlx::query!(
            r#"SELECT application_id AS "application_id!",state AS "state!" FROM operations WHERE id=?1"#,
            operation_id
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| StoreError::Database)?
        .ok_or(StoreError::NotFound)?;
        let operation_state = WorkState::parse(&operation.state)?;
        if operation.application_id != application_id_value || operation_state.terminal() {
            return Err(StoreError::InvalidInput);
        }

        let application = sqlx::query_as!(
            ApplicationRow,
            r#"SELECT id AS "id!",name AS "name!",desired_json AS "desired_json!",resolved_json AS "resolved_json!",deployed_json,generation AS "generation!",spec_hash AS "spec_hash!",delete_intent AS "delete_intent!",created_at_ms AS "created_at_ms!",updated_at_ms AS "updated_at_ms!" FROM applications WHERE id=?1"#,
            application_id_value
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| StoreError::Database)?
        .ok_or(StoreError::NotFound)?;
        let application = application.decode(&self.instance_id)?;
        if !application
            .application
            .spec
            .services
            .iter()
            .any(|service| service.name == service_name)
        {
            return Err(StoreError::InvalidInput);
        }

        let id = new_id("build");
        let now = now_ms();
        let inserted = sqlx::query!(
            "INSERT OR IGNORE INTO builds(id,operation_id,application_id,service_name,state,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,'pending',?5,?5)",
            id,
            operation_id,
            application_id_value,
            service_name,
            now
        )
        .execute(&mut *tx)
        .await
        .map_err(|_| StoreError::Database)?
        .rows_affected();
        if inserted != 1 {
            return Err(StoreError::AlreadyExists);
        }
        tx.commit().await.map_err(|_| StoreError::Database)?;
        Ok(Build {
            id,
            operation_id: operation_id.into(),
            application_id: application_id.clone(),
            service_name: service_name.into(),
            state: WorkState::Pending,
            source_commit: None,
            image_reference: None,
            image_digest: None,
            error_code: None,
            error_message: None,
            created_at_ms: now,
            updated_at_ms: now,
            started_at_ms: None,
            finished_at_ms: None,
        })
    }

    async fn build(&self, id: &str) -> Result<Build, StoreError> {
        let row = sqlx::query_as!(
            BuildRow,
            r#"SELECT id AS "id!",operation_id AS "operation_id!",application_id AS "application_id!",service_name AS "service_name!",state AS "state!",source_commit,image_reference,image_digest,error_code,error_message,created_at_ms AS "created_at_ms!",updated_at_ms AS "updated_at_ms!",started_at_ms,finished_at_ms FROM builds WHERE id=?1"#,
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| StoreError::Database)?
        .ok_or(StoreError::NotFound)?;
        parse_build_row(row)
    }

    async fn builds_for_operation(&self, operation_id: &str) -> Result<Vec<Build>, StoreError> {
        let rows = sqlx::query_as!(
            BuildRow,
            r#"SELECT id AS "id!",operation_id AS "operation_id!",application_id AS "application_id!",service_name AS "service_name!",state AS "state!",source_commit,image_reference,image_digest,error_code,error_message,created_at_ms AS "created_at_ms!",updated_at_ms AS "updated_at_ms!",started_at_ms,finished_at_ms FROM builds WHERE operation_id=?1 ORDER BY service_name,id"#,
            operation_id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|_| StoreError::Database)?;
        rows.into_iter().map(parse_build_row).collect()
    }

    async fn record_build_output(
        &self,
        id: &str,
        source_commit: &str,
        image_reference: &str,
        image_digest: &str,
    ) -> Result<(), StoreError> {
        if source_commit.is_empty()
            || image_reference.is_empty()
            || image_digest.is_empty()
            || [source_commit, image_reference, image_digest]
                .iter()
                .any(|value| value.len() > 512 || value.chars().any(char::is_control))
        {
            return Err(StoreError::InvalidInput);
        }
        let mut connection = self.connection().await?;
        let now = now_ms();
        let changed = sqlx::query!(
            "UPDATE builds SET source_commit=?1,image_reference=?2,image_digest=?3,updated_at_ms=?4 WHERE id=?5 AND state='running' AND (source_commit IS NULL OR source_commit=?1) AND (image_reference IS NULL OR image_reference=?2) AND (image_digest IS NULL OR image_digest=?3) AND EXISTS (SELECT 1 FROM operations WHERE id=builds.operation_id AND state='running')",
            source_commit,
            image_reference,
            image_digest,
            now,
            id
        )
        .execute(&mut *connection)
        .await
        .map_err(|_| StoreError::Database)?
        .rows_affected();
        if changed == 1 {
            Ok(())
        } else {
            Err(Self::transition_miss(&mut connection, "builds", id).await?)
        }
    }

    async fn transition_build(
        &self,
        id: &str,
        from: WorkState,
        to: WorkState,
        error: Option<(&str, &str)>,
    ) -> Result<(), StoreError> {
        if !from.can_transition_to(to) {
            return Err(StoreError::IllegalTransition);
        }
        if error.is_some() != (to == WorkState::Failed) {
            return Err(StoreError::IllegalTransition);
        }
        if !valid_error(error) {
            return Err(StoreError::InvalidInput);
        }
        let now = now_ms();
        let (error_code, error_message) = error.map_or((None, None), |(a, b)| (Some(a), Some(b)));
        let to_state = to.as_str();
        let from_state = from.as_str();
        let started = (to == WorkState::Running).then_some(now);
        let finished = to.terminal().then_some(now);
        let mut connection = self.connection().await?;
        let changed = sqlx::query!(
            "UPDATE builds SET state=?1,error_code=?2,error_message=?3,updated_at_ms=?4,started_at_ms=COALESCE(started_at_ms,?5),finished_at_ms=?6 WHERE id=?7 AND state=?8 AND EXISTS (SELECT 1 FROM operations WHERE id=builds.operation_id AND state='running') AND (?1 != 'succeeded' OR (source_commit IS NOT NULL AND image_reference IS NOT NULL AND image_digest IS NOT NULL))",
            to_state,
            error_code,
            error_message,
            now,
            started,
            finished,
            id,
            from_state
        )
        .execute(&mut *connection)
        .await
        .map_err(|_| StoreError::Database)?
        .rows_affected();
        if changed == 1 {
            Ok(())
        } else {
            Err(Self::transition_miss(&mut connection, "builds", id).await?)
        }
    }

    async fn prune_finished_before(&self, cutoff_ms: i64, limit: u32) -> Result<u64, StoreError> {
        let limit = i64::from(limit);
        sqlx::query!(
            "DELETE FROM builds WHERE id IN (SELECT id FROM builds WHERE finished_at_ms IS NOT NULL AND finished_at_ms < ?1 ORDER BY finished_at_ms LIMIT ?2)",
            cutoff_ms,
            limit
        )
        .execute(&self.pool)
        .await
        .map_err(|_| StoreError::Database)
        .map(|result| result.rows_affected())
    }
}

#[async_trait]
impl BuildArtifactRepository for SqliteStore {
    async fn record_build_identity(
        &self,
        build_id: &str,
        build_key: &str,
        context_hash: &str,
    ) -> Result<(), StoreError> {
        if !valid_sha256(build_key) || !valid_sha256(context_hash) {
            return Err(StoreError::InvalidInput);
        }
        let changed = sqlx::query!(
            "INSERT INTO build_artifacts(build_id,build_key,context_hash,verified) SELECT ?1,?2,?3,0 WHERE EXISTS (SELECT 1 FROM builds WHERE id=?1 AND state IN ('running','recovery')) ON CONFLICT(build_id) DO UPDATE SET build_key=excluded.build_key,context_hash=excluded.context_hash,verified=0,verified_at_ms=NULL WHERE build_artifacts.verified=0",
            build_id,
            build_key,
            context_hash
        )
        .execute(&self.pool)
        .await
        .map_err(|_| StoreError::Database)?
        .rows_affected();
        if changed == 1 {
            Ok(())
        } else {
            let mut connection = self.connection().await?;
            Err(Self::transition_miss(&mut connection, "builds", build_id).await?)
        }
    }

    async fn mark_build_verified(&self, build_id: &str) -> Result<(), StoreError> {
        let now = now_ms();
        let changed = sqlx::query!(
            "UPDATE build_artifacts SET verified=1,verified_at_ms=?1 WHERE build_id=?2 AND verified=0 AND EXISTS (SELECT 1 FROM builds WHERE id=?2 AND state='running' AND source_commit IS NOT NULL AND image_reference IS NOT NULL AND image_digest IS NOT NULL)",
            now,
            build_id
        )
        .execute(&self.pool)
        .await
        .map_err(|_| StoreError::Database)?
        .rows_affected();
        if changed == 1 {
            Ok(())
        } else {
            let mut connection = self.connection().await?;
            Err(Self::transition_miss(&mut connection, "builds", build_id).await?)
        }
    }

    async fn verified_build_for_key(&self, build_key: &str) -> Result<Option<Build>, StoreError> {
        if !valid_sha256(build_key) {
            return Err(StoreError::InvalidInput);
        }
        let row = sqlx::query_as!(
            BuildRow,
            r#"SELECT b.id AS "id!",b.operation_id AS "operation_id!",b.application_id AS "application_id!",b.service_name AS "service_name!",b.state AS "state!",b.source_commit,b.image_reference,b.image_digest,b.error_code,b.error_message,b.created_at_ms AS "created_at_ms!",b.updated_at_ms AS "updated_at_ms!",b.started_at_ms,b.finished_at_ms FROM builds b JOIN build_artifacts a ON a.build_id=b.id JOIN applications app ON app.id=b.application_id WHERE a.build_key=?1 AND a.verified=1 AND b.state='succeeded' AND b.image_digest IS NOT NULL AND app.deleted_at_ms IS NULL LIMIT 1"#,
            build_key
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| StoreError::Database)?;
        row.map(parse_build_row).transpose()
    }

    async fn build_artifact(&self, build_id: &str) -> Result<BuildArtifact, StoreError> {
        let row = sqlx::query!(
            r#"SELECT build_id AS "build_id!",build_key AS "build_key!",context_hash AS "context_hash!",verified AS "verified!",verified_at_ms FROM build_artifacts WHERE build_id=?1"#,
            build_id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| StoreError::Database)?
        .ok_or(StoreError::NotFound)?;
        Ok(BuildArtifact {
            build_id: row.build_id,
            build_key: row.build_key,
            context_hash: row.context_hash,
            verified: row.verified == 1,
            verified_at_ms: row.verified_at_ms,
        })
    }

    async fn append_build_log(
        &self,
        build_id: &str,
        sequence: u64,
        timestamp_ms: i64,
        message: &str,
    ) -> Result<(), StoreError> {
        if sequence == 0 || timestamp_ms <= 0 || !valid_bounded_text(message, 16_384) {
            return Err(StoreError::InvalidInput);
        }
        let sequence = i64::try_from(sequence).map_err(|_| StoreError::InvalidInput)?;
        let mut tx = self.begin_immediate().await?;
        let retained = sqlx::query_scalar!(
            r#"SELECT COALESCE(SUM(length(message)),0) AS "retained!: i64" FROM build_logs WHERE build_id=?1"#,
            build_id
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| StoreError::Database)?;
        let message_len = i64::try_from(message.len()).map_err(|_| StoreError::InvalidInput)?;
        if retained.saturating_add(message_len) <= 4 * 1024 * 1024 {
            let inserted = sqlx::query!(
                "INSERT INTO build_logs(build_id,sequence,timestamp_ms,message) SELECT ?1,?2,?3,?4 WHERE EXISTS (SELECT 1 FROM builds WHERE id=?1) ON CONFLICT(build_id,sequence) DO NOTHING",
                build_id,
                sequence,
                timestamp_ms,
                message
            )
            .execute(&mut *tx)
            .await
            .map_err(|_| StoreError::Database)?
            .rows_affected();
            if inserted != 1 {
                return Err(StoreError::IllegalTransition);
            }
        }
        tx.commit().await.map_err(|_| StoreError::Database)
    }

    async fn build_logs(
        &self,
        build_id: &str,
        after: u64,
        limit: u32,
    ) -> Result<Vec<BuildLog>, StoreError> {
        if limit == 0 || limit > 1000 {
            return Err(StoreError::InvalidInput);
        }
        let after = i64::try_from(after).map_err(|_| StoreError::InvalidInput)?;
        let limit = i64::from(limit);
        let exists = sqlx::query_scalar!(
            r#"SELECT 1 AS "present!: i64" FROM builds WHERE id=?1"#,
            build_id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| StoreError::Database)?
        .is_some();
        if !exists {
            return Err(StoreError::NotFound);
        }
        let rows = sqlx::query!(
            r#"SELECT build_id AS "build_id!",sequence AS "sequence!",timestamp_ms AS "timestamp_ms!",message AS "message!" FROM build_logs WHERE build_id=?1 AND sequence>?2 ORDER BY sequence LIMIT ?3"#,
            build_id,
            after,
            limit
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|_| StoreError::Database)?;
        rows.into_iter()
            .map(|row| {
                Ok(BuildLog {
                    build_id: row.build_id,
                    sequence: u64::try_from(row.sequence).map_err(|_| StoreError::Corrupt)?,
                    timestamp_ms: row.timestamp_ms,
                    message: row.message,
                })
            })
            .collect()
    }
}
