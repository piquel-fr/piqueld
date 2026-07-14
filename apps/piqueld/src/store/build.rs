//! Build repository implementation.

use super::{
    ApplicationId, ApplicationRow, Build, BuildRepository, SqliteStore, StoreError, WorkState,
    async_trait, decode_stored_application, new_id, now_ms, valid_error,
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
            r#"SELECT id AS "id!",name AS "name!",desired_json AS "desired_json!",resolved_json,generation AS "generation!",spec_hash AS "spec_hash!",delete_intent AS "delete_intent!",created_at_ms AS "created_at_ms!",updated_at_ms AS "updated_at_ms!" FROM applications WHERE id=?1"#,
            application_id_value
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| StoreError::Database)?
        .ok_or(StoreError::NotFound)?;
        let application = decode_stored_application(application, &self.instance_id)?;
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
