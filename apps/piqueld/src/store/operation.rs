//! Operation repository implementation.

use super::{
    ApplicationId, Operation, OperationError, OperationKind, OperationStep, SqliteStore, StepState,
    StoreError, WorkState, now_ms, page_limit,
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

impl SqliteStore {
    /// Atomically completes a successful delete operation and hides its application.
    pub(crate) async fn finish_delete_operation(
        &self,
        operation: &Operation,
    ) -> Result<(), StoreError> {
        let now = now_ms();
        let generation = super::generation_i64(operation.generation)?;
        let application_id = operation.application_id.as_str();
        let mut tx = self.begin_immediate().await?;
        let changed = sqlx::query!(
            "UPDATE operations SET state='succeeded',error_code=NULL,error_message=NULL,updated_at_ms=?1,finished_at_ms=?1 WHERE id=?2 AND application_id=?3 AND generation=?4 AND kind='delete' AND state='running' AND NOT EXISTS (SELECT 1 FROM operation_steps WHERE operation_id=?2 AND state NOT IN ('succeeded','skipped'))",
            now,
            operation.id,
            application_id,
            generation
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        if changed != 1 {
            return Err(StoreError::IllegalTransition);
        }
        Self::finalize_delete_in_transaction(
            &mut tx,
            &operation.application_id,
            operation.generation,
            now,
        )
        .await?;
        tx.commit().await.map_err(StoreError::database)
    }

    async fn recover_operation(&self, operation_id: &str, now: i64) -> Result<(), StoreError> {
        let mut tx = self.begin_immediate().await?;
        let changed = sqlx::query!(
            "UPDATE operations SET state='recovery',error_code=NULL,error_message=NULL,updated_at_ms=?1,started_at_ms=NULL,finished_at_ms=NULL WHERE id=?2 AND state='running'",
            now,
            operation_id
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?
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
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }

    async fn finish_operation(
        &self,
        operation_id: &str,
        from: WorkState,
        to: WorkState,
        error: Option<&OperationError>,
        now: i64,
    ) -> Result<(), StoreError> {
        let to_state = to.as_str();
        let from_state = from.as_str();
        let error_code = error.map(OperationError::code);
        let error_message = error.map(OperationError::message);
        let started = (to == WorkState::Running).then_some(now);
        let mut tx = self.begin_immediate().await?;
        sqlx::query!(
            "UPDATE operation_steps SET state='cancelled',error_code=NULL,error_message=NULL,updated_at_ms=?1,finished_at_ms=?1 WHERE operation_id=?2 AND state IN ('pending','running','recovery')",
            now,
            operation_id
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
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
        .map_err(StoreError::database)?
        .rows_affected();
        if changed != 1 {
            return Err(Self::transition_miss(&mut tx, "operations", operation_id).await?);
        }
        tx.commit().await.map_err(StoreError::database)
    }
}

impl Operation {
    fn parse_row(row: OperationRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: row.id,
            application_id: ApplicationId::parse(row.application_id)
                .map_err(StoreError::corrupt)?,
            generation: u64::try_from(row.generation).map_err(StoreError::corrupt)?,
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
    action: String,
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
        Ok(Self {
            id: row.id,
            operation_id: row.operation_id,
            position: u32::try_from(row.position).map_err(StoreError::corrupt)?,
            action: row.action,
            state: StepState::parse(row.state.as_str())?,
            attempt: u32::try_from(row.attempt).map_err(StoreError::corrupt)?,
            error_code: row.error_code,
            error_message: row.error_message,
            created_at_ms: row.created_at_ms,
            updated_at_ms: row.updated_at_ms,
            started_at_ms: row.started_at_ms,
            finished_at_ms: row.finished_at_ms,
        })
    }
}

impl SqliteStore {
    /// Reads an operation and its steps from one consistent database snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the operation is missing, malformed, or the
    /// database transaction cannot be read.
    pub async fn operation_with_steps(
        &self,
        operation_id: &str,
    ) -> Result<(Operation, Vec<OperationStep>), StoreError> {
        let mut tx = self.pool.begin().await.map_err(StoreError::database)?;
        let operation = sqlx::query_as!(
            OperationRow,
            r#"SELECT id AS "id!",application_id AS "application_id!",generation AS "generation!",kind AS "kind!",state AS "state!",error_code,error_message,created_at_ms AS "created_at_ms!",updated_at_ms AS "updated_at_ms!",started_at_ms,finished_at_ms FROM operations WHERE id=?1"#,
            operation_id
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)
        .and_then(Operation::parse_row)?;
        let steps = sqlx::query_as!(
            OperationStepRow,
            r#"SELECT id AS "id!",operation_id AS "operation_id!",position AS "position!",action AS "action!",state AS "state!",attempt AS "attempt!",error_code,error_message,created_at_ms AS "created_at_ms!",updated_at_ms AS "updated_at_ms!",started_at_ms,finished_at_ms FROM operation_steps WHERE operation_id=?1 ORDER BY position"#,
            operation_id
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .into_iter()
        .map(OperationStep::parse_step_row)
        .collect::<Result<Vec<_>, _>>()?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok((operation, steps))
    }

    /// Reads the most recently created operation for an application.
    ///
    /// The dashboard uses this bounded lookup to show current command progress
    /// without exposing a general operation-list endpoint.
    ///
    /// # Errors
    /// Returns [`StoreError`] when the operation query or row decoding fails.
    pub async fn latest_operation_for_application(
        &self,
        application_id: &ApplicationId,
    ) -> Result<Option<(Operation, Vec<OperationStep>)>, StoreError> {
        let operation_id = sqlx::query_scalar::<_, String>(
            "SELECT id FROM operations WHERE application_id=?1 ORDER BY created_at_ms DESC,id DESC LIMIT 1",
        )
        .bind(application_id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?;
        let Some(operation_id) = operation_id else {
            return Ok(None);
        };
        self.operation_with_steps(&operation_id).await.map(Some)
    }
}

impl SqliteStore {
    pub(crate) async fn operation(&self, operation_id: &str) -> Result<Operation, StoreError> {
        let row = sqlx::query_as!(
            OperationRow,
            r#"SELECT id AS "id!",application_id AS "application_id!",generation AS "generation!",kind AS "kind!",state AS "state!",error_code,error_message,created_at_ms AS "created_at_ms!",updated_at_ms AS "updated_at_ms!",started_at_ms,finished_at_ms FROM operations WHERE id=?1"#,
            operation_id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        Operation::parse_row(row)
    }

    pub(crate) async fn operation_steps(
        &self,
        operation_id: &str,
    ) -> Result<Vec<OperationStep>, StoreError> {
        let rows = sqlx::query_as!(
            OperationStepRow,
            r#"SELECT id AS "id!",operation_id AS "operation_id!",position AS "position!",action AS "action!",state AS "state!",attempt AS "attempt!",error_code,error_message,created_at_ms AS "created_at_ms!",updated_at_ms AS "updated_at_ms!",started_at_ms,finished_at_ms FROM operation_steps WHERE operation_id=?1 ORDER BY position"#,
            operation_id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?;
        rows.into_iter()
            .map(OperationStep::parse_step_row)
            .collect()
    }

    pub(crate) async fn pending_operations(
        &self,
        limit: usize,
    ) -> Result<Vec<Operation>, StoreError> {
        // Only expose the oldest queued generation for each application. This
        // makes ordering durable rather than depending on task scheduling or
        // mutex acquisition order in one process.
        let limit = page_limit(limit)?;
        let rows = sqlx::query_as!(
            OperationRow,
            r#"SELECT o.id AS "id!",o.application_id AS "application_id!",o.generation AS "generation!",o.kind AS "kind!",o.state AS "state!",o.error_code,o.error_message,o.created_at_ms AS "created_at_ms!",o.updated_at_ms AS "updated_at_ms!",o.started_at_ms,o.finished_at_ms FROM operations o WHERE o.state IN ('pending','recovery') AND NOT EXISTS (SELECT 1 FROM operations older WHERE older.application_id=o.application_id AND older.state IN ('pending','recovery','running') AND (older.generation < o.generation OR (older.generation=o.generation AND (older.created_at_ms < o.created_at_ms OR (older.created_at_ms=o.created_at_ms AND older.id < o.id))))) ORDER BY o.created_at_ms,o.id LIMIT ?1"#,
            limit
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?;
        rows.into_iter().map(Operation::parse_row).collect()
    }

    pub(crate) async fn transition_operation(
        &self,
        operation_id: &str,
        from: WorkState,
        to: WorkState,
        error: Option<crate::operations::OperationError>,
    ) -> Result<(), StoreError> {
        if !from.can_transition_to(to) {
            return Err(StoreError::IllegalTransition);
        }
        if error.is_some() != (to == WorkState::Failed) {
            return Err(StoreError::IllegalTransition);
        }
        let error_code = error.as_ref().map(OperationError::code);
        let error_message = error.as_ref().map(OperationError::message);
        let now = now_ms();
        let finished = to.terminal().then_some(now);
        let started = (to == WorkState::Running).then_some(now);
        let to_state = to.as_str();
        let from_state = from.as_str();

        if from == WorkState::Running && to == WorkState::Recovery {
            return self.recover_operation(operation_id, now).await;
        }

        if matches!(to, WorkState::Failed | WorkState::Cancelled) {
            return self
                .finish_operation(operation_id, from, to, error.as_ref(), now)
                .await;
        }

        let mut connection = self.connection().await?;
        let changed = sqlx::query!(
            "UPDATE operations SET state=?1,error_code=?2,error_message=?3,updated_at_ms=?4,started_at_ms=COALESCE(started_at_ms,?5),finished_at_ms=?6 WHERE id=?7 AND state=?8 AND (?1 != 'running' OR NOT EXISTS (SELECT 1 FROM operations active WHERE active.application_id=(SELECT target.application_id FROM operations target WHERE target.id=?7) AND active.state='running' AND active.id != ?7)) AND (?1 != 'succeeded' OR NOT EXISTS (SELECT 1 FROM operation_steps WHERE operation_id=?7 AND state NOT IN ('succeeded','skipped')))",
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
        .map_err(StoreError::database)?
        .rows_affected();
        if changed == 1 {
            Ok(())
        } else {
            Err(Self::transition_miss(&mut connection, "operations", operation_id).await?)
        }
    }
    pub(crate) async fn transition_step(
        &self,
        step_id: &str,
        from: StepState,
        to: StepState,
        error: Option<OperationError>,
    ) -> Result<(), StoreError> {
        if !from.can_transition_to(to) {
            return Err(StoreError::IllegalTransition);
        }
        if error.is_some() != (to == StepState::Failed) {
            return Err(StoreError::IllegalTransition);
        }
        let now = now_ms();
        let error_code = error.as_ref().map(OperationError::code);
        let error_message = error.as_ref().map(OperationError::message);
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
        .map_err(StoreError::database)?
        .rows_affected();
        if changed == 1 {
            Ok(())
        } else {
            Err(Self::transition_miss(&mut connection, "operation_steps", step_id).await?)
        }
    }

    pub(crate) async fn recover_interrupted(&self) -> Result<u64, StoreError> {
        let mut tx = self.begin_immediate().await?;
        let now = now_ms();
        let mut count = sqlx::query!(
            "UPDATE operations SET state='recovery',updated_at_ms=?1,started_at_ms=NULL WHERE state='running'",
            now
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        count += sqlx::query!(
            "UPDATE operation_steps SET state='recovery',updated_at_ms=?1,started_at_ms=NULL WHERE state='running'",
            now
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        tx.commit().await.map_err(StoreError::database)?;
        Ok(count)
    }
}
