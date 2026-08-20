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
    /// Atomically completes a delete operation and finalizes its application.
    ///
    /// The operation must be running, belong to the delete operation kind, and have
    /// only succeeded or skipped steps. Otherwise, the transition fails with
    /// `StoreError::IllegalTransition`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(
    /// #     store: &SqliteStore,
    /// #     operation: &Operation,
    /// # ) -> Result<(), StoreError> {
    /// store.finish_delete_operation(operation).await?;
    /// # Ok(())
    /// # }
    /// ```
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

    /// Moves a running operation and its running steps into recovery state.
    ///
    /// Clears error details and timing fields while recording the recovery timestamp.
    ///
    pub(crate) async fn recover_operation(&self, operation_id: &str, now: i64) -> Result<(), StoreError> {
    /// # Examples
    ///
    /// ```no_run
    /// store.recover_operation("operation-id", current_time_ms).await?;
    /// # Ok::<(), StoreError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if the operation cannot be updated, does not exist, or is not running.
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

    /// Completes an operation state transition and cancels its unfinished steps.
    ///
    /// The operation is updated only when it is currently in `from`. When transitioning
    /// to `running`, the operation's start time is initialized if needed.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// store
    ///     .finish_operation(
    ///         operation_id,
    ///         WorkState::Running,
    ///         WorkState::Succeeded,
    ///         None,
    ///         now,
    ///     )
    ///     .await?;
    /// # Ok::<(), StoreError>(())
    /// ```
    ///
    /// # Parameters
    ///
    /// * `operation_id` identifies the operation to update.
    /// * `from` is the operation's expected current state.
    /// * `to` is the operation's new state.
    /// * `error` provides error details to record with the new state.
    /// * `now` is the timestamp used for operation and step updates.
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
    /// Converts a database row into an operation, validating identifiers, generation, kind, and state.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// let operation = Operation::parse_row(row)?;
    /// assert_eq!(operation.id, row.id);
    /// # Ok::<(), StoreError>(())
    /// ```
    ///
    /// Returns a corruption error when a database value cannot be converted into its
    /// corresponding domain value.
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
    /// Converts a database row into an operation step, validating its position, attempt count, and state.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::corrupt`] when the row contains invalid numeric values or an unrecognized state.
    ///
    /// # Examples
    ///
    /// ```
    /// # let row: OperationStepRow = todo!();
    /// let step = OperationStep::parse_step_row(row)?;
    /// # Ok::<(), StoreError>(())
    /// ```
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
    /// Reads an operation and its steps from a consistent database snapshot.
    ///
    /// # Arguments
    ///
    /// * `operation_id` — Identifier of the operation to retrieve.
    ///
    /// # Returns
    ///
    /// The parsed operation and its steps ordered by position.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] if the operation does not exist, a corruption
    /// error if stored data is malformed, or a database error if the transaction
    /// cannot be read or committed.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(store: &SqliteStore) -> Result<(), StoreError> {
    /// let (operation, steps) = store.operation_with_steps("operation-id").await?;
    /// # let _ = (operation, steps);
    /// # Ok(())
    /// # }
    /// ```
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
}

impl SqliteStore {
    /// Loads an operation by its identifier.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] when no matching operation exists, or another
    /// [`StoreError`] if the database row cannot be read or parsed.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(store: &SqliteStore) -> Result<(), StoreError> {
    /// let operation = store.operation("operation-id").await?;
    /// # let _ = operation;
    /// # Ok(())
    /// # }
    /// ```
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

    /// Retrieves all steps for an operation in position order.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(store: &SqliteStore) -> Result<(), StoreError> {
    /// let steps = store.operation_steps("operation-id").await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Returns a database or parsing error when the steps cannot be retrieved or
    /// contain invalid data.
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

    /// Lists pending and recovery operations, exposing only the oldest eligible generation for each application.
    ///
    /// # Examples
    ///
    /// ```
    /// # async fn example(store: &SqliteStore) -> Result<(), StoreError> {
    /// let operations = store.pending_operations(10).await?;
    /// # Ok(())
    /// # }
    /// ```
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

    /// Transitions an operation between valid workflow states while enforcing state-specific constraints.
    ///
    /// Transitions to `recovery`, `failed`, or `cancelled` apply the corresponding recovery or
    /// completion behavior. A successful transition requires all operation steps to be terminal, and
    /// only one operation for an application may be running at a time.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::IllegalTransition` when the state change or error value is invalid.
    /// Returns a transition error when the operation does not currently have the expected state.
    ///
    /// # Examples
    ///
    /// ```
    /// # async fn example(store: &SqliteStore) -> Result<(), StoreError> {
    /// store
    ///     .transition_operation(
    ///         "operation-id",
    ///         WorkState::Pending,
    ///         WorkState::Running,
    ///         None,
    ///     )
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
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
    /// Transitions an operation step between valid states while enforcing execution order and concurrency rules.
    ///
    /// A step can start only when its operation is running, no other step is running, and all preceding
    /// steps have reached a terminal state. Failed transitions require an error.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::IllegalTransition`] when the state change or error value is invalid.
    /// Returns a store error when the step cannot be updated or the expected source state no longer
    /// applies.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(store: &SqliteStore) -> Result<(), StoreError> {
    /// store
    ///     .transition_step(
    ///         "step-id",
    ///         StepState::Running,
    ///         StepState::Succeeded,
    ///         None,
    ///     )
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Returns
    ///
    /// `Ok(())` when the step is transitioned successfully.
    ///
    /// [`StoreError::IllegalTransition`]: crate::store::StoreError::IllegalTransition
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

    /// Moves all running operations and operation steps to recovery and clears their start timestamps.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(store: &SqliteStore) -> Result<(), StoreError> {
    /// let recovered = store.recover_interrupted().await?;
    /// println!("Recovered {recovered} records");
    /// # Ok(())
    /// # }
    /// ```
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
