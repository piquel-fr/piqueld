//! Operation repository implementation.

use super::{
    ApplicationId, Operation, OperationKind, OperationRepository, OperationStep, SqliteStore,
    StepState, StoreError, WorkState, async_trait, now_ms, valid_error,
};

#[derive(Debug)]
struct OperationRow {
    id: String,
    application_id: String,
    generation: i64,
    kind: String,
    state: String,
    error_code: Option<String>,
    error_message: Option<String>,
    created_at_ms: i64,
    updated_at_ms: i64,
    started_at_ms: Option<i64>,
    finished_at_ms: Option<i64>,
}

impl Operation {
    fn parse_row(row: OperationRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: row.id,
            application_id: ApplicationId::parse(row.application_id)
                .map_err(|_| StoreError::Corrupt)?,
            generation: u64::try_from(row.generation).map_err(|_| StoreError::Corrupt)?,
            kind: OperationKind::parse(&row.kind)?,
            state: WorkState::parse(&row.state)?,
            error_code: row.error_code,
            error_message: row.error_message,
            created_at_ms: row.created_at_ms,
            updated_at_ms: row.updated_at_ms,
            started_at_ms: row.started_at_ms,
            finished_at_ms: row.finished_at_ms,
        })
    }
}

#[derive(Debug)]
struct OperationStepRow {
    id: String,
    operation_id: String,
    position: i64,
    kind: String,
    state: String,
    attempt: i64,
    error_code: Option<String>,
    error_message: Option<String>,
    created_at_ms: i64,
    updated_at_ms: i64,
    started_at_ms: Option<i64>,
    finished_at_ms: Option<i64>,
}

impl OperationStep {
    fn parse_step_row(row: OperationStepRow) -> Result<Self, StoreError> {
        let state = match row.state.as_str() {
            "pending" => StepState::Pending,
            "running" => StepState::Running,
            "recovery" => StepState::Recovery,
            "succeeded" => StepState::Succeeded,
            "failed" => StepState::Failed,
            "cancelled" => StepState::Cancelled,
            "skipped" => StepState::Skipped,
            _ => return Err(StoreError::Corrupt),
        };
        Ok(Self {
            id: row.id,
            operation_id: row.operation_id,
            position: u32::try_from(row.position).map_err(|_| StoreError::Corrupt)?,
            kind: row.kind,
            state,
            attempt: u32::try_from(row.attempt).map_err(|_| StoreError::Corrupt)?,
            error_code: row.error_code,
            error_message: row.error_message,
            created_at_ms: row.created_at_ms,
            updated_at_ms: row.updated_at_ms,
            started_at_ms: row.started_at_ms,
            finished_at_ms: row.finished_at_ms,
        })
    }
}

#[async_trait]
impl OperationRepository for SqliteStore {
    async fn operation(&self, operation_id: &str) -> Result<Operation, StoreError> {
        let row = sqlx::query_as!(
            OperationRow,
            r#"SELECT id AS "id!",application_id AS "application_id!",generation AS "generation!",kind AS "kind!",state AS "state!",error_code,error_message,created_at_ms AS "created_at_ms!",updated_at_ms AS "updated_at_ms!",started_at_ms,finished_at_ms FROM operations WHERE id=?1"#,
            operation_id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| StoreError::Database)?
        .ok_or(StoreError::NotFound)?;
        Operation::parse_row(row)
    }

    async fn operation_steps(&self, operation_id: &str) -> Result<Vec<OperationStep>, StoreError> {
        let rows = sqlx::query_as!(
            OperationStepRow,
            r#"SELECT id AS "id!",operation_id AS "operation_id!",position AS "position!",kind AS "kind!",state AS "state!",attempt AS "attempt!",error_code,error_message,created_at_ms AS "created_at_ms!",updated_at_ms AS "updated_at_ms!",started_at_ms,finished_at_ms FROM operation_steps WHERE operation_id=?1 ORDER BY position"#,
            operation_id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|_| StoreError::Database)?;
        rows.into_iter()
            .map(OperationStep::parse_step_row)
            .collect()
    }

    async fn operations_for_application(
        &self,
        application_id: &ApplicationId,
        limit: u32,
    ) -> Result<Vec<Operation>, StoreError> {
        let application_id = application_id.as_str();
        let limit = i64::from(limit);
        let rows = sqlx::query_as!(
            OperationRow,
            r#"SELECT id AS "id!",application_id AS "application_id!",generation AS "generation!",kind AS "kind!",state AS "state!",error_code,error_message,created_at_ms AS "created_at_ms!",updated_at_ms AS "updated_at_ms!",started_at_ms,finished_at_ms FROM operations WHERE application_id=?1 ORDER BY generation DESC,created_at_ms DESC,id DESC LIMIT ?2"#,
            application_id,
            limit
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|_| StoreError::Database)?;
        rows.into_iter().map(Operation::parse_row).collect()
    }

    async fn pending_operations(&self, limit: u32) -> Result<Vec<Operation>, StoreError> {
        // Only expose the oldest queued generation for each application. This
        // makes ordering durable rather than depending on task scheduling or
        // mutex acquisition order in one process.
        let limit = i64::from(limit);
        let rows = sqlx::query_as!(
            OperationRow,
            r#"SELECT o.id AS "id!",o.application_id AS "application_id!",o.generation AS "generation!",o.kind AS "kind!",o.state AS "state!",o.error_code,o.error_message,o.created_at_ms AS "created_at_ms!",o.updated_at_ms AS "updated_at_ms!",o.started_at_ms,o.finished_at_ms FROM operations o WHERE o.state IN ('pending','recovery') AND NOT EXISTS (SELECT 1 FROM operations older WHERE older.application_id=o.application_id AND older.state IN ('pending','recovery','running') AND (older.generation < o.generation OR (older.generation=o.generation AND (older.created_at_ms < o.created_at_ms OR (older.created_at_ms=o.created_at_ms AND older.id < o.id))))) ORDER BY o.created_at_ms,o.id LIMIT ?1"#,
            limit
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|_| StoreError::Database)?;
        rows.into_iter().map(Operation::parse_row).collect()
    }

    #[allow(clippy::too_many_lines)]
    async fn transition_operation(
        &self,
        operation_id: &str,
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
        let finished = to.terminal().then_some(now);
        let started = (to == WorkState::Running).then_some(now);
        let to_state = to.as_str();
        let from_state = from.as_str();

        if from == WorkState::Running && to == WorkState::Recovery {
            let mut tx = self.begin_immediate().await?;
            let changed = sqlx::query!(
                "UPDATE operations SET state='recovery',error_code=NULL,error_message=NULL,updated_at_ms=?1,started_at_ms=NULL,finished_at_ms=NULL WHERE id=?2 AND state='running'",
                now,
                operation_id
            )
            .execute(&mut *tx)
            .await
            .map_err(|_| StoreError::Database)?
            .rows_affected();
            if changed != 1 {
                return Err(Self::transition_miss(&mut tx, "operations", operation_id).await?);
            }
            sqlx::query!(
                "UPDATE operation_steps SET state='recovery',error_code=NULL,error_message=NULL,updated_at_ms=?1,started_at_ms=NULL,finished_at_ms=NULL WHERE operation_id=?2 AND state='running'",
                now,
                operation_id
            )
            .execute(&mut *tx)
            .await
            .map_err(|_| StoreError::Database)?;
            sqlx::query!(
                "UPDATE builds SET state='recovery',error_code=NULL,error_message=NULL,updated_at_ms=?1,started_at_ms=NULL,finished_at_ms=NULL WHERE operation_id=?2 AND state='running'",
                now,
                operation_id
            )
            .execute(&mut *tx)
            .await
            .map_err(|_| StoreError::Database)?;
            tx.commit().await.map_err(|_| StoreError::Database)?;
            return Ok(());
        }

        if matches!(to, WorkState::Failed | WorkState::Cancelled) {
            let mut tx = self.begin_immediate().await?;
            sqlx::query!(
                "UPDATE operation_steps SET state='cancelled',error_code=NULL,error_message=NULL,updated_at_ms=?1,finished_at_ms=?1 WHERE operation_id=?2 AND state IN ('pending','running','recovery')",
                now,
                operation_id
            )
            .execute(&mut *tx)
            .await
            .map_err(|_| StoreError::Database)?;
            sqlx::query!(
                "UPDATE builds SET state='cancelled',error_code=NULL,error_message=NULL,updated_at_ms=?1,finished_at_ms=?1 WHERE operation_id=?2 AND state IN ('pending','running','recovery')",
                now,
                operation_id
            )
            .execute(&mut *tx)
            .await
            .map_err(|_| StoreError::Database)?;
            let changed = sqlx::query!(
                "UPDATE operations SET state=?1,error_code=?2,error_message=?3,updated_at_ms=?4,started_at_ms=COALESCE(started_at_ms,?5),finished_at_ms=?4 WHERE id=?6 AND state=?7",
                to_state,
                error_code,
                error_message,
                now,
                started,
                operation_id,
                from_state
            )
            .execute(&mut *tx)
            .await
            .map_err(|_| StoreError::Database)?
            .rows_affected();
            if changed != 1 {
                return Err(Self::transition_miss(&mut tx, "operations", operation_id).await?);
            }
            tx.commit().await.map_err(|_| StoreError::Database)?;
            return Ok(());
        }

        let mut connection = self.connection().await?;
        let changed = sqlx::query!(
            "UPDATE operations SET state=?1,error_code=?2,error_message=?3,updated_at_ms=?4,started_at_ms=COALESCE(started_at_ms,?5),finished_at_ms=?6 WHERE id=?7 AND state=?8 AND (?1 != 'running' OR NOT EXISTS (SELECT 1 FROM operations active WHERE active.application_id=(SELECT target.application_id FROM operations target WHERE target.id=?7) AND active.state='running' AND active.id != ?7)) AND (?1 NOT IN ('failed','cancelled') OR (NOT EXISTS (SELECT 1 FROM operation_steps WHERE operation_id=?7 AND state='running') AND NOT EXISTS (SELECT 1 FROM builds WHERE operation_id=?7 AND state='running'))) AND (?1 != 'succeeded' OR (NOT EXISTS (SELECT 1 FROM operation_steps WHERE operation_id=?7 AND state NOT IN ('succeeded','skipped')) AND NOT EXISTS (SELECT 1 FROM builds WHERE operation_id=?7 AND state != 'succeeded')))",
            to_state,
            error_code,
            error_message,
            now,
            started,
            finished,
            operation_id,
            from_state
        )
        .execute(&mut *connection)
        .await
        .map_err(|_| StoreError::Database)?
        .rows_affected();
        if changed == 1 {
            Ok(())
        } else {
            Err(Self::transition_miss(&mut connection, "operations", operation_id).await?)
        }
    }

    async fn transition_step(
        &self,
        step_id: &str,
        from: StepState,
        to: StepState,
        error: Option<(&str, &str)>,
    ) -> Result<(), StoreError> {
        if !from.can_transition_to(to) {
            return Err(StoreError::IllegalTransition);
        }
        if error.is_some() != (to == StepState::Failed) {
            return Err(StoreError::IllegalTransition);
        }
        if !valid_error(error) {
            return Err(StoreError::InvalidInput);
        }
        let now = now_ms();
        let (error_code, error_message) = error.map_or((None, None), |(a, b)| (Some(a), Some(b)));
        let finished = to.terminal().then_some(now);
        let started = (to == StepState::Running).then_some(now);
        let attempt = i64::from(to == StepState::Running && from != StepState::Running);
        let to_state = to.as_str();
        let from_state = from.as_str();
        let mut connection = self.connection().await?;
        let changed = sqlx::query!(
            "UPDATE operation_steps SET state=?1,error_code=?2,error_message=?3,updated_at_ms=?4,started_at_ms=COALESCE(started_at_ms,?5),finished_at_ms=?6,attempt=attempt+?7 WHERE id=?8 AND state=?9 AND EXISTS (SELECT 1 FROM operations WHERE id=operation_steps.operation_id AND state='running') AND (?1 != 'running' OR (NOT EXISTS (SELECT 1 FROM operation_steps active WHERE active.operation_id=operation_steps.operation_id AND active.state='running' AND active.id != operation_steps.id) AND NOT EXISTS (SELECT 1 FROM operation_steps earlier WHERE earlier.operation_id=operation_steps.operation_id AND earlier.position < operation_steps.position AND earlier.state NOT IN ('succeeded','skipped'))))",
            to_state,
            error_code,
            error_message,
            now,
            started,
            finished,
            attempt,
            step_id,
            from_state
        )
        .execute(&mut *connection)
        .await
        .map_err(|_| StoreError::Database)?
        .rows_affected();
        if changed == 1 {
            Ok(())
        } else {
            Err(Self::transition_miss(&mut connection, "operation_steps", step_id).await?)
        }
    }

    async fn recover_interrupted(&self) -> Result<u64, StoreError> {
        let mut tx = self.begin_immediate().await?;
        let now = now_ms();
        let mut count = sqlx::query!(
            "UPDATE operations SET state='recovery',updated_at_ms=?1,started_at_ms=NULL WHERE state='running'",
            now
        )
        .execute(&mut *tx)
        .await
        .map_err(|_| StoreError::Database)?
        .rows_affected();
        count += sqlx::query!(
            "UPDATE operation_steps SET state='recovery',updated_at_ms=?1,started_at_ms=NULL WHERE state='running'",
            now
        )
        .execute(&mut *tx)
        .await
        .map_err(|_| StoreError::Database)?
        .rows_affected();
        count += sqlx::query!(
            "UPDATE builds SET state='recovery',updated_at_ms=?1,started_at_ms=NULL WHERE state='running'",
            now
        )
        .execute(&mut *tx)
        .await
        .map_err(|_| StoreError::Database)?
        .rows_affected();
        tx.commit().await.map_err(|_| StoreError::Database)?;
        Ok(count)
    }

    async fn prune_finished_operations_before(
        &self,
        cutoff_ms: i64,
        limit: u32,
    ) -> Result<u64, StoreError> {
        let limit = i64::from(limit);
        sqlx::query!(
            "DELETE FROM operations WHERE id IN (SELECT id FROM operations WHERE finished_at_ms IS NOT NULL AND finished_at_ms < ?1 AND NOT EXISTS (SELECT 1 FROM application_create_idempotency i WHERE i.operation_id=operations.id) ORDER BY finished_at_ms,id LIMIT ?2)",
            cutoff_ms,
            limit
        )
        .execute(&self.pool)
        .await
        .map_err(|_| StoreError::Database)
        .map(|result| result.rows_affected())
    }
}
