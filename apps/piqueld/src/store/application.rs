//! Application repository implementation.

use super::{
    ApplicationId, ApplicationPage, ApplicationRow, MutationResult, NormalizedApplication,
    OperationKind, ResolvedApplication, Sqlite, SqliteStore, StoreError, StoredApplication,
    Transaction, canonical_resolved, generation_i64, now_ms, page_limit, valid_sha256,
    validate_operation_steps,
};
use uuid::Uuid;

struct IdempotencyBinding<'a> {
    key_hash: &'a str,
    request_hash: &'a str,
    application_id: &'a str,
    operation_id: &'a str,
    generation: i64,
    kind: OperationKind,
    created_at_ms: i64,
}

struct IdempotencyReplay {
    mutation: MutationResult,
    operation_state: Option<String>,
}

impl IdempotencyReplay {
    fn is_dead(&self) -> bool {
        matches!(
            self.operation_state.as_deref(),
            Some("failed" | "cancelled")
        )
    }
}

impl SqliteStore {
    /// Creates an application, its initial status row, and a durable operation.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when validation fails, the application already
    /// exists, or the transaction cannot be committed.
    pub async fn create(
        &self,
        app: &NormalizedApplication,
        resolved: &ResolvedApplication,
        steps: &[String],
    ) -> Result<MutationResult, StoreError> {
        let mut tx = self.begin_immediate().await?;
        let desired = app.canonical_json().map_err(StoreError::corrupt)?;
        let resolved = canonical_resolved(app, resolved, &self.instance_id)?;
        let hash = app.spec_hash();
        let now = now_ms();
        let app_id = app.id.as_str();
        let app_name = app.metadata.name.as_str();
        let inserted = sqlx::query!(
            "INSERT INTO applications(id,name,generation,desired_json,resolved_json,spec_hash,created_at_ms,updated_at_ms) VALUES(?1,?2,1,?3,?4,?5,?6,?6) ON CONFLICT DO NOTHING",
            app_id,
            app_name,
            desired,
            resolved,
            hash,
            now
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        if inserted != 1 {
            return Err(StoreError::AlreadyExists);
        }
        sqlx::query!(
            "INSERT INTO application_status(application_id,state,updated_at_ms) VALUES(?1,'pending',?2)",
            app_id,
            now
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let operation_id =
            Self::insert_operation(&mut tx, &app.id, 1, OperationKind::Create, steps, now).await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(MutationResult {
            generation: 1,
            operation_id,
        })
    }

    /// Creates an application and binds the first request to an idempotency key.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the key is invalid, its binding conflicts, or
    /// the transaction cannot be committed.
    pub async fn create_idempotent(
        &self,
        app: &NormalizedApplication,
        resolved: &ResolvedApplication,
        steps: &[String],
        key_hash: &str,
        request_hash: &str,
    ) -> Result<MutationResult, StoreError> {
        if !valid_sha256(key_hash) || !valid_sha256(request_hash) {
            return Err(StoreError::InvalidInput);
        }
        let mut tx = self.begin_immediate().await?;
        let existing = sqlx::query!(
            r#"SELECT request_hash AS "request_hash!",application_id AS "application_id!",operation_id AS "operation_id!",generation AS "generation!",kind AS "kind!" FROM mutation_idempotency WHERE key_hash=?1"#,
            key_hash
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        if let Some(row) = existing {
            if row.request_hash != request_hash
                || row.application_id != app.id.as_str()
                || row.kind != OperationKind::Create.as_str()
            {
                return Err(StoreError::IdempotencyConflict);
            }
            let generation = u64::try_from(row.generation).map_err(StoreError::corrupt)?;
            if generation != 1 {
                return Err(StoreError::Corrupt);
            }
            tx.commit().await.map_err(StoreError::database)?;
            return Ok(MutationResult {
                generation,
                operation_id: row.operation_id,
            });
        }

        let desired = app.canonical_json().map_err(StoreError::corrupt)?;
        let resolved = canonical_resolved(app, resolved, &self.instance_id)?;
        let hash = app.spec_hash();
        let now = now_ms();
        let app_id = app.id.as_str();
        let app_name = app.metadata.name.as_str();
        let inserted = sqlx::query!(
            "INSERT INTO applications(id,name,generation,desired_json,resolved_json,spec_hash,created_at_ms,updated_at_ms) VALUES(?1,?2,1,?3,?4,?5,?6,?6) ON CONFLICT DO NOTHING",
            app_id,
            app_name,
            desired,
            resolved,
            hash,
            now
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        if inserted != 1 {
            return Err(StoreError::AlreadyExists);
        }
        sqlx::query!(
            "INSERT INTO application_status(application_id,state,updated_at_ms) VALUES(?1,'pending',?2)",
            app_id,
            now
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let operation_id =
            Self::insert_operation(&mut tx, &app.id, 1, OperationKind::Create, steps, now).await?;
        sqlx::query!(
            "INSERT INTO mutation_idempotency(key_hash,request_hash,application_id,operation_id,generation,kind,created_at_ms) VALUES(?1,?2,?3,?4,1,'create',?5)",
            key_hash,
            request_hash,
            app_id,
            operation_id,
            now
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(MutationResult {
            generation: 1,
            operation_id,
        })
    }

    /// Looks up an existing create request by its hashed idempotency key.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the hashes are invalid, the binding conflicts,
    /// or the database cannot be read.
    pub async fn create_idempotency(
        &self,
        app_id: &ApplicationId,
        key_hash: &str,
        request_hash: &str,
    ) -> Result<Option<MutationResult>, StoreError> {
        if !valid_sha256(key_hash) || !valid_sha256(request_hash) {
            return Err(StoreError::InvalidInput);
        }
        let row = sqlx::query!(
            r#"SELECT request_hash AS "request_hash!",application_id AS "application_id!",operation_id AS "operation_id!",generation AS "generation!",kind AS "kind!" FROM mutation_idempotency WHERE key_hash=?1"#,
            key_hash
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?;
        let Some(row) = row else {
            return Ok(None);
        };
        if row.request_hash != request_hash
            || row.application_id != app_id.as_str()
            || row.kind != OperationKind::Create.as_str()
        {
            return Err(StoreError::IdempotencyConflict);
        }
        let generation = u64::try_from(row.generation).map_err(StoreError::corrupt)?;
        if generation != 1 {
            return Err(StoreError::Corrupt);
        }
        Ok(Some(MutationResult {
            generation,
            operation_id: row.operation_id,
        }))
    }

    /// Looks up an idempotency binding for any application mutation.
    ///
    /// # Errors
    /// Returns [`StoreError`] when hashes or the requested operation kind are
    /// invalid, the key was bound to different intent, or the database cannot
    /// be read.
    pub async fn mutation_idempotency(
        &self,
        app_id: &ApplicationId,
        key_hash: &str,
        request_hash: &str,
        kind: OperationKind,
    ) -> Result<Option<MutationResult>, StoreError> {
        if !valid_sha256(key_hash) || !valid_sha256(request_hash) {
            return Err(StoreError::InvalidInput);
        }
        let row = sqlx::query!(
            r#"SELECT request_hash AS "request_hash!",application_id AS "application_id!",operation_id AS "operation_id!",generation AS "generation!",kind AS "kind!" FROM mutation_idempotency WHERE key_hash=?1"#,
            key_hash
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?;
        let Some(row) = row else {
            return Ok(None);
        };
        if row.request_hash != request_hash
            || row.application_id != app_id.as_str()
            || row.kind != kind.as_str()
        {
            return Err(StoreError::IdempotencyConflict);
        }
        Ok(Some(MutationResult {
            generation: u64::try_from(row.generation).map_err(StoreError::corrupt)?,
            operation_id: row.operation_id,
        }))
    }

    /// Replaces an application generation and records the replacement operation.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for stale generations, invalid state, corrupt
    /// persisted data, or a failed transaction.
    pub async fn replace(
        &self,
        app: &NormalizedApplication,
        resolved: &ResolvedApplication,
        expected_generation: u64,
        steps: &[String],
    ) -> Result<MutationResult, StoreError> {
        self.replace_with_key(app, resolved, expected_generation, steps, None)
            .await
    }

    /// Replaces an application generation and binds the mutation to an
    /// idempotency key when one is supplied.
    ///
    /// # Errors
    /// Returns [`StoreError`] for stale generations, invalid state, idempotency
    /// conflicts, corrupt persisted data, or a failed transaction.
    pub async fn replace_idempotent(
        &self,
        app: &NormalizedApplication,
        resolved: &ResolvedApplication,
        expected_generation: u64,
        steps: &[String],
        key_hash: &str,
        request_hash: &str,
    ) -> Result<MutationResult, StoreError> {
        self.replace_with_key(
            app,
            resolved,
            expected_generation,
            steps,
            Some((key_hash, request_hash)),
        )
        .await
    }

    async fn replace_with_key(
        &self,
        app: &NormalizedApplication,
        resolved: &ResolvedApplication,
        expected_generation: u64,
        steps: &[String],
        idempotency: Option<(&str, &str)>,
    ) -> Result<MutationResult, StoreError> {
        if let Some((key_hash, request_hash)) = idempotency
            && (!valid_sha256(key_hash) || !valid_sha256(request_hash))
        {
            return Err(StoreError::InvalidInput);
        }
        let mut tx = self.begin_immediate().await?;
        if let Some((key_hash, request_hash)) = idempotency {
            let replay = Self::find_idempotency(
                &mut tx,
                key_hash,
                request_hash,
                app.id.as_str(),
                OperationKind::Replace,
            )
            .await?;
            match replay {
                Some(replay) if !replay.is_dead() => {
                    tx.commit().await.map_err(StoreError::database)?;
                    return Ok(replay.mutation);
                }
                Some(replay) => {
                    let app_id = app.id.as_str();
                    let replay_generation = generation_i64(replay.mutation.generation)?;
                    let current = sqlx::query_scalar!(
                        r#"SELECT COUNT(*) AS "count!: i64" FROM applications WHERE id=?1 AND generation=?2 AND delete_intent=0 AND deleted_at_ms IS NULL"#,
                        app_id,
                        replay_generation
                    )
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(StoreError::database)?;
                    if current != 1 {
                        tx.commit().await.map_err(StoreError::database)?;
                        return Ok(replay.mutation);
                    }
                    let result = Self::reset_failed_replace(
                        &mut tx,
                        &app.id,
                        replay.mutation.generation,
                        steps,
                        now_ms(),
                    )
                    .await?;
                    tx.commit().await.map_err(StoreError::database)?;
                    return Ok(result);
                }
                None => {}
            }
        }
        let generation = expected_generation
            .checked_add(1)
            .ok_or(StoreError::Corrupt)?;
        let now = now_ms();
        let app_id = app.id.as_str();
        let new_generation = generation_i64(generation)?;
        self.replace_application_row(&mut tx, app, resolved, expected_generation, generation, now)
            .await?;
        Self::set_application_status(&mut tx, app_id, "pending", now).await?;
        let operation_id = Self::insert_operation(
            &mut tx,
            &app.id,
            generation,
            OperationKind::Replace,
            steps,
            now,
        )
        .await?;
        if let Some((key_hash, request_hash)) = idempotency {
            Self::bind_idempotency(
                &mut tx,
                IdempotencyBinding {
                    key_hash,
                    request_hash,
                    application_id: app_id,
                    operation_id: &operation_id,
                    generation: new_generation,
                    kind: OperationKind::Replace,
                    created_at_ms: now,
                },
            )
            .await?;
        }
        tx.commit().await.map_err(StoreError::database)?;
        Ok(MutationResult {
            generation,
            operation_id,
        })
    }

    /// Queues an explicit reconciliation for the current application generation.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for a stale generation, deleting application, or
    /// failed durable write.
    pub async fn request_reconcile(
        &self,
        id: &ApplicationId,
        expected_generation: u64,
        steps: &[String],
    ) -> Result<MutationResult, StoreError> {
        let mut tx = self.begin_immediate().await?;
        let id_value = id.as_str();
        let expected_generation_i64 = generation_i64(expected_generation)?;
        let row = sqlx::query!(
            "SELECT generation,delete_intent FROM applications WHERE id=?1",
            id_value
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        let actual = u64::try_from(row.generation).map_err(StoreError::corrupt)?;
        if actual != expected_generation {
            return Err(StoreError::GenerationConflict {
                expected: expected_generation,
                actual,
            });
        }
        if row.delete_intent == 1 {
            return Err(StoreError::IllegalTransition);
        }
        let existing = sqlx::query_scalar!(
            r#"SELECT id AS "id!" FROM operations WHERE application_id=?1 AND generation=?2 AND kind='reconcile' AND state IN ('pending','running','recovery') ORDER BY created_at_ms DESC,id DESC LIMIT 1"#,
            id_value,
            expected_generation_i64
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        if let Some(operation_id) = existing {
            tx.commit().await.map_err(StoreError::database)?;
            return Ok(MutationResult {
                generation: expected_generation,
                operation_id,
            });
        }
        let now = now_ms();
        let operation_id = Self::insert_operation(
            &mut tx,
            id,
            expected_generation,
            OperationKind::Reconcile,
            steps,
            now,
        )
        .await?;
        let status_changed = sqlx::query!(
            "UPDATE application_status SET state='pending',observed_generation=NULL,message=NULL,updated_at_ms=?1 WHERE application_id=?2",
            now,
            id_value
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        if status_changed != 1 {
            return Err(StoreError::Corrupt);
        }
        tx.commit().await.map_err(StoreError::database)?;
        Ok(MutationResult {
            generation: expected_generation,
            operation_id,
        })
    }

    /// Returns the active reconciliation for a generation, when one exists.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the generation is not representable or the
    /// journal cannot be read.
    pub async fn active_reconcile(
        &self,
        id: &ApplicationId,
        expected_generation: u64,
    ) -> Result<Option<MutationResult>, StoreError> {
        let id_value = id.as_str();
        let expected_generation_i64 = generation_i64(expected_generation)?;
        let operation_id = sqlx::query_scalar!(
            r#"SELECT id AS "id!" FROM operations WHERE application_id=?1 AND generation=?2 AND kind='reconcile' AND state IN ('pending','running','recovery') ORDER BY created_at_ms DESC,id DESC LIMIT 1"#,
            id_value,
            expected_generation_i64
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?;
        Ok(operation_id.map(|operation_id| MutationResult {
            generation: expected_generation,
            operation_id,
        }))
    }

    /// Marks an application for deletion and records its delete operation.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for a stale generation, illegal lifecycle state, or
    /// failed durable write.
    pub async fn request_delete(
        &self,
        id: &ApplicationId,
        expected_generation: u64,
        steps: &[String],
    ) -> Result<MutationResult, StoreError> {
        self.request_delete_with_key(id, expected_generation, steps, None)
            .await
    }

    /// Marks an application for deletion and binds the mutation to an
    /// idempotency key when one is supplied.
    ///
    /// # Errors
    /// Returns [`StoreError`] for stale generations, illegal lifecycle state,
    /// idempotency conflicts, or a failed durable write.
    pub async fn request_delete_idempotent(
        &self,
        id: &ApplicationId,
        expected_generation: u64,
        steps: &[String],
        key_hash: &str,
        request_hash: &str,
    ) -> Result<MutationResult, StoreError> {
        self.request_delete_with_key(
            id,
            expected_generation,
            steps,
            Some((key_hash, request_hash)),
        )
        .await
    }

    async fn request_delete_with_key(
        &self,
        id: &ApplicationId,
        expected_generation: u64,
        steps: &[String],
        idempotency: Option<(&str, &str)>,
    ) -> Result<MutationResult, StoreError> {
        if let Some((key_hash, request_hash)) = idempotency
            && (!valid_sha256(key_hash) || !valid_sha256(request_hash))
        {
            return Err(StoreError::InvalidInput);
        }
        let id_value = id.as_str();
        let mut tx = self.begin_immediate().await?;
        let now = now_ms();
        if let Some(mutation) =
            Self::replay_or_resurrect_delete(&mut tx, idempotency, id, steps, now).await?
        {
            tx.commit().await.map_err(StoreError::database)?;
            return Ok(mutation);
        }
        let expected_generation_i64 = generation_i64(expected_generation)?;
        let row = sqlx::query!(
            "SELECT generation AS \"generation!\",delete_intent AS \"delete_intent!\" FROM applications WHERE id=?1 AND deleted_at_ms IS NULL",
            id_value
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        let actual = u64::try_from(row.generation).map_err(StoreError::corrupt)?;
        if actual != expected_generation {
            return Err(StoreError::GenerationConflict {
                expected: expected_generation,
                actual,
            });
        }
        if row.delete_intent == 1 {
            let operation = sqlx::query!(
                r#"SELECT id AS "id!",state AS "state!" FROM operations WHERE application_id=?1 AND generation=?2 AND kind='delete' ORDER BY created_at_ms DESC,id DESC LIMIT 1"#,
                id_value,
                expected_generation_i64
            )
            .fetch_optional(&mut *tx)
            .await
            .map_err(StoreError::database)?
            .ok_or(StoreError::Corrupt)?;
            let result = match operation.state.as_str() {
                "pending" | "running" | "recovery" => MutationResult {
                    generation: expected_generation,
                    operation_id: operation.id,
                },
                "failed" | "cancelled" => {
                    Self::reset_failed_delete(&mut tx, id, expected_generation, steps, now).await?
                }
                _ => return Err(StoreError::IllegalTransition),
            };
            Self::bind_optional_idempotency(
                &mut tx,
                idempotency,
                id_value,
                &result,
                expected_generation_i64,
                OperationKind::Delete,
                now,
            )
            .await?;
            tx.commit().await.map_err(StoreError::database)?;
            return Ok(result);
        }
        let generation = expected_generation
            .checked_add(1)
            .ok_or(StoreError::Corrupt)?;
        let generation_i64 = generation_i64(generation)?;
        Self::mark_application_deleting(
            &mut tx,
            id_value,
            expected_generation,
            generation_i64,
            now,
        )
        .await?;
        Self::set_application_status(&mut tx, id_value, "deleting", now).await?;
        let operation_id =
            Self::insert_operation(&mut tx, id, generation, OperationKind::Delete, steps, now)
                .await?;
        let result = MutationResult {
            generation,
            operation_id,
        };
        Self::bind_optional_idempotency(
            &mut tx,
            idempotency,
            id_value,
            &result,
            generation_i64,
            OperationKind::Delete,
            now,
        )
        .await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(result)
    }

    /// Tombstones an application after its delete operation has succeeded.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the generation is invalid, the delete
    /// operation has not succeeded, or the tombstone cannot be written.
    pub async fn finalize_delete(
        &self,
        id: &ApplicationId,
        generation: u64,
    ) -> Result<(), StoreError> {
        let now = now_ms();
        let mut tx = self.begin_immediate().await?;
        Self::finalize_delete_in_transaction(&mut tx, id, generation, now).await?;
        tx.commit().await.map_err(StoreError::database)
    }

    pub(super) async fn finalize_delete_in_transaction(
        tx: &mut Transaction<'_, Sqlite>,
        id: &ApplicationId,
        generation: u64,
        now: i64,
    ) -> Result<(), StoreError> {
        let id_value = id.as_str();
        let generation_i64 = generation_i64(generation)?;
        let operation_exists = sqlx::query_scalar!(
            r#"SELECT 1 AS "present!: i64" FROM operations WHERE application_id=?1 AND generation=?2 AND kind='delete' AND state='succeeded'"#,
            id_value,
            generation_i64
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)?
        .is_some();
        if !operation_exists {
            return Err(StoreError::IllegalTransition);
        }

        let tombstone_name = loop {
            let candidate = format!("deleted-{}-{now}-{}", id.as_str(), Uuid::now_v7().simple());
            let candidate_name = candidate.as_str();
            let exists = sqlx::query_scalar!(
                r#"SELECT 1 AS "present!: i64" FROM applications WHERE name=?1"#,
                candidate_name
            )
            .fetch_optional(&mut **tx)
            .await
            .map_err(StoreError::database)?
            .is_some();
            if !exists {
                break candidate;
            }
        };
        let changed = sqlx::query!(
            "UPDATE applications SET name=?1,deleted_at_ms=?2,updated_at_ms=?2 WHERE id=?3 AND generation=?4 AND delete_intent=1 AND deleted_at_ms IS NULL",
            tombstone_name,
            now,
            id_value,
            generation_i64
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        if changed == 1 {
            sqlx::query!(
                "DELETE FROM application_status WHERE application_id=?1",
                id_value
            )
            .execute(&mut **tx)
            .await
            .map_err(StoreError::database)?;
            Ok(())
        } else {
            Err(StoreError::IllegalTransition)
        }
    }

    /// Reads one live application and revalidates its persisted state.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] for an absent or tombstoned application,
    /// or another store error when persisted state fails validation.
    pub async fn get(&self, id: &ApplicationId) -> Result<StoredApplication, StoreError> {
        let id_value = id.as_str();
        let row = sqlx::query_as!(
            ApplicationRow,
            r#"SELECT id AS "id!",name AS "name!",desired_json AS "desired_json!",resolved_json AS "resolved_json!",generation AS "generation!",spec_hash AS "spec_hash!",delete_intent AS "delete_intent!",created_at_ms AS "created_at_ms!",updated_at_ms AS "updated_at_ms!" FROM applications WHERE id=?1 AND deleted_at_ms IS NULL"#,
            id_value
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        let stored = row.decode(&self.instance_id)?;
        if stored.application.id != *id {
            return Err(StoreError::Corrupt);
        }
        Ok(stored)
    }

    /// Finds one live application by its user-facing name.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the row cannot be read or revalidated.
    pub async fn find_by_name(&self, name: &str) -> Result<Option<StoredApplication>, StoreError> {
        let row = sqlx::query_as!(
            ApplicationRow,
            r#"SELECT id AS "id!",name AS "name!",desired_json AS "desired_json!",resolved_json AS "resolved_json!",generation AS "generation!",spec_hash AS "spec_hash!",delete_intent AS "delete_intent!",created_at_ms AS "created_at_ms!",updated_at_ms AS "updated_at_ms!" FROM applications WHERE name=?1 AND deleted_at_ms IS NULL"#,
            name
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?;
        row.map(|row| row.decode(&self.instance_id)).transpose()
    }

    /// Lists live applications in stable identifier order.
    ///
    /// Undecodable rows are quarantined: each is logged with its application
    /// id and skipped so one corrupt row cannot fail the whole page. Reads of
    /// a single application stay fail-closed.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when pagination is invalid or rows cannot be
    /// read.
    ///
    /// # Panics
    ///
    /// This method cannot produce an empty page cursor; the internal assertion
    /// would indicate a database/query invariant violation.
    pub async fn list(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<ApplicationPage, StoreError> {
        let fetch_limit = page_limit(limit)? + 1;
        let after = cursor
            .map(|cursor| {
                cursor
                    .strip_prefix("v1:")
                    .ok_or(StoreError::InvalidInput)
                    .and_then(|id| ApplicationId::parse(id).map_err(StoreError::invalid_input))
            })
            .transpose()?;
        let rows = if let Some(after) = after {
            let after = after.as_str();
            sqlx::query_as!(
                ApplicationRow,
                r#"SELECT id AS "id!",name AS "name!",desired_json AS "desired_json!",resolved_json AS "resolved_json!",generation AS "generation!",spec_hash AS "spec_hash!",delete_intent AS "delete_intent!",created_at_ms AS "created_at_ms!",updated_at_ms AS "updated_at_ms!" FROM applications WHERE id > ?1 AND deleted_at_ms IS NULL ORDER BY id LIMIT ?2"#,
                after,
                fetch_limit
            )
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query_as!(
                ApplicationRow,
                r#"SELECT id AS "id!",name AS "name!",desired_json AS "desired_json!",resolved_json AS "resolved_json!",generation AS "generation!",spec_hash AS "spec_hash!",delete_intent AS "delete_intent!",created_at_ms AS "created_at_ms!",updated_at_ms AS "updated_at_ms!" FROM applications WHERE deleted_at_ms IS NULL ORDER BY id LIMIT ?1"#,
                fetch_limit
            )
            .fetch_all(&self.pool)
            .await
        }
        .map_err(StoreError::database)?;
        let mut items = Vec::with_capacity(rows.len());
        // Quarantined rows still count toward the page size: one undecodable
        // row inside a full page must not suppress `next_cursor` and hide
        // every later application.
        let mut rows_fetched = 0_usize;
        let mut last_row_id: Option<String> = None;
        for row in rows {
            rows_fetched += 1;
            last_row_id = Some(row.id.clone());
            let id = row.id.clone();
            match row.decode(&self.instance_id) {
                Ok(stored) => items.push(stored),
                Err(error) => tracing::error!(
                    application_id = %id,
                    error = %error,
                    "quarantined undecodable application row"
                ),
            }
        }
        let has_more =
            usize::try_from(fetch_limit).is_ok_and(|fetch_limit| rows_fetched == fetch_limit);
        items.truncate(limit);
        let next_cursor = has_more.then(|| {
            let cursor_id = items
                .last()
                .map(|stored| stored.application.id.to_string())
                .or_else(|| last_row_id.clone())
                .expect("a bounded page carrying more rows has a cursor");
            format!("v1:{cursor_id}")
        });
        Ok(ApplicationPage { items, next_cursor })
    }
}

impl SqliteStore {
    async fn mark_application_deleting(
        tx: &mut Transaction<'_, Sqlite>,
        application_id: &str,
        expected_generation: u64,
        generation: i64,
        now: i64,
    ) -> Result<(), StoreError> {
        let expected_generation_db = generation_i64(expected_generation)?;
        let changed = sqlx::query!(
            "UPDATE applications SET generation=?1,delete_intent=1,updated_at_ms=?2 WHERE id=?3 AND generation=?4 AND delete_intent=0",
            generation,
            now,
            application_id,
            expected_generation_db
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        if changed == 1 {
            return Ok(());
        }
        let row = sqlx::query!(
            "SELECT generation,delete_intent FROM applications WHERE id=?1",
            application_id
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        let actual = u64::try_from(row.generation).map_err(StoreError::corrupt)?;
        if actual == expected_generation && row.delete_intent == 1 {
            Err(StoreError::IllegalTransition)
        } else {
            Err(StoreError::GenerationConflict {
                expected: expected_generation,
                actual,
            })
        }
    }

    async fn set_application_status(
        tx: &mut Transaction<'_, Sqlite>,
        application_id: &str,
        state: &str,
        now: i64,
    ) -> Result<(), StoreError> {
        let changed = sqlx::query!(
            "UPDATE application_status SET state=?1,observed_generation=NULL,message=NULL,updated_at_ms=?2 WHERE application_id=?3",
            state,
            now,
            application_id
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        if changed == 1 {
            Ok(())
        } else {
            Err(StoreError::Corrupt)
        }
    }

    async fn replace_application_row(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        app: &NormalizedApplication,
        resolved: &ResolvedApplication,
        expected_generation: u64,
        generation: u64,
        now: i64,
    ) -> Result<(), StoreError> {
        let desired = app.canonical_json().map_err(StoreError::corrupt)?;
        let resolved = canonical_resolved(app, resolved, &self.instance_id)?;
        let app_name = app.metadata.name.as_str();
        let new_generation = generation_i64(generation)?;
        let spec_hash = app.spec_hash();
        let application_id = app.id.as_str();
        let expected_generation_db = generation_i64(expected_generation)?;
        let changed = sqlx::query!(
            "UPDATE applications SET name=?1,generation=?2,desired_json=?3,resolved_json=?4,spec_hash=?5,updated_at_ms=?6 WHERE id=?7 AND generation=?8 AND delete_intent=0",
            app_name,
            new_generation,
            desired,
            resolved,
            spec_hash,
            now,
            application_id,
            expected_generation_db
        )
        .execute(&mut **tx)
        .await
        .map_err(|error| {
            if error.as_database_error().is_some_and(sqlx::error::DatabaseError::is_unique_violation)
            {
                // A rename colliding with another live name is an existence
                // conflict; constraint failures must not be silently ignored.
                StoreError::AlreadyExists
            } else {
                StoreError::database(error)
            }
        })?
        .rows_affected();
        if changed == 1 {
            return Ok(());
        }
        let row = sqlx::query!(
            "SELECT generation,delete_intent FROM applications WHERE id=?1",
            application_id
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        let actual = u64::try_from(row.generation).map_err(StoreError::corrupt)?;
        if actual != expected_generation {
            return Err(StoreError::GenerationConflict {
                expected: expected_generation,
                actual,
            });
        }
        if row.delete_intent == 1 {
            Err(StoreError::IllegalTransition)
        } else {
            Err(StoreError::AlreadyExists)
        }
    }

    async fn find_idempotency(
        tx: &mut Transaction<'_, Sqlite>,
        key_hash: &str,
        request_hash: &str,
        application_id: &str,
        kind: OperationKind,
    ) -> Result<Option<IdempotencyReplay>, StoreError> {
        let row = sqlx::query!(
            r#"SELECT m.request_hash AS "request_hash!",m.application_id AS "application_id!",m.operation_id AS "operation_id!",m.generation AS "generation!",m.kind AS "kind!",o.state AS "operation_state?" FROM mutation_idempotency m LEFT JOIN operations o ON o.id=m.operation_id WHERE m.key_hash=?1"#,
            key_hash
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        let Some(row) = row else {
            return Ok(None);
        };
        if row.request_hash != request_hash
            || row.application_id != application_id
            || row.kind != kind.as_str()
        {
            return Err(StoreError::IdempotencyConflict);
        }
        Ok(Some(IdempotencyReplay {
            mutation: MutationResult {
                generation: u64::try_from(row.generation).map_err(StoreError::corrupt)?,
                operation_id: row.operation_id,
            },
            operation_state: row.operation_state,
        }))
    }

    async fn bind_idempotency(
        tx: &mut Transaction<'_, Sqlite>,
        binding: IdempotencyBinding<'_>,
    ) -> Result<(), StoreError> {
        let kind = binding.kind.as_str();
        sqlx::query!(
            "INSERT INTO mutation_idempotency(key_hash,request_hash,application_id,operation_id,generation,kind,created_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            binding.key_hash,
            binding.request_hash,
            binding.application_id,
            binding.operation_id,
            binding.generation,
            kind,
            binding.created_at_ms,
        )
        .execute(&mut **tx)
        .await
        .map_err(|error| {
            if error.as_database_error().is_some_and(sqlx::error::DatabaseError::is_unique_violation)
            {
                // Another request bound this key first; treat it as a replay
                // conflict instead of an opaque storage failure.
                StoreError::IdempotencyConflict
            } else {
                StoreError::database(error)
            }
        })?;
        Ok(())
    }

    async fn bind_optional_idempotency(
        tx: &mut Transaction<'_, Sqlite>,
        idempotency: Option<(&str, &str)>,
        application_id: &str,
        mutation: &MutationResult,
        generation: i64,
        kind: OperationKind,
        created_at_ms: i64,
    ) -> Result<(), StoreError> {
        if let Some((key_hash, request_hash)) = idempotency {
            Self::bind_idempotency(
                tx,
                IdempotencyBinding {
                    key_hash,
                    request_hash,
                    application_id,
                    operation_id: &mutation.operation_id,
                    generation,
                    kind,
                    created_at_ms,
                },
            )
            .await?;
        }
        Ok(())
    }

    /// Returns the verbatim replay for a live delete binding, resurrects a
    /// dead one in place under its existing binding, or returns `None` when
    /// the key is unbound.
    async fn replay_or_resurrect_delete(
        tx: &mut Transaction<'_, Sqlite>,
        idempotency: Option<(&str, &str)>,
        id: &ApplicationId,
        steps: &[String],
        now: i64,
    ) -> Result<Option<MutationResult>, StoreError> {
        let Some((key_hash, request_hash)) = idempotency else {
            return Ok(None);
        };
        let replay = Self::find_idempotency(
            tx,
            key_hash,
            request_hash,
            id.as_str(),
            OperationKind::Delete,
        )
        .await?;
        match replay {
            Some(replay) if !replay.is_dead() => Ok(Some(replay.mutation)),
            // A dead binding must not pin its failed operation forever.
            Some(replay) => {
                let result =
                    Self::reset_failed_delete(tx, id, replay.mutation.generation, steps, now)
                        .await?;
                Ok(Some(result))
            }
            None => Ok(None),
        }
    }

    async fn reset_failed_delete(
        tx: &mut Transaction<'_, Sqlite>,
        id: &ApplicationId,
        generation: u64,
        steps: &[String],
        now: i64,
    ) -> Result<MutationResult, StoreError> {
        validate_operation_steps(steps)?;
        let generation = generation_i64(generation)?;
        let id_value = id.as_str();
        let operation = sqlx::query!(
            r#"SELECT id AS "id!",state AS "state!" FROM operations WHERE application_id=?1 AND generation=?2 AND kind='delete' ORDER BY created_at_ms DESC,id DESC LIMIT 1"#,
            id_value,
            generation
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::Corrupt)?;
        if !matches!(operation.state.as_str(), "failed" | "cancelled") {
            return Err(StoreError::IllegalTransition);
        }
        sqlx::query!(
            "UPDATE operations SET state='pending',error_code=NULL,error_message=NULL,updated_at_ms=?1,started_at_ms=NULL,finished_at_ms=NULL WHERE id=?2 AND state IN ('failed','cancelled')",
            now,
            operation.id
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        sqlx::query!(
            "DELETE FROM operation_steps WHERE operation_id=?1",
            operation.id
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Self::insert_operation_steps(tx, &operation.id, steps, now).await?;
        let status_changed = sqlx::query!(
            "UPDATE application_status SET state='deleting',observed_generation=NULL,message=NULL,updated_at_ms=?1 WHERE application_id=?2",
            now,
            id_value
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        if status_changed != 1 {
            return Err(StoreError::Corrupt);
        }
        Ok(MutationResult {
            generation: u64::try_from(generation).map_err(StoreError::corrupt)?,
            operation_id: operation.id,
        })
    }

    async fn reset_failed_replace(
        tx: &mut Transaction<'_, Sqlite>,
        id: &ApplicationId,
        generation: u64,
        steps: &[String],
        now: i64,
    ) -> Result<MutationResult, StoreError> {
        validate_operation_steps(steps)?;
        let generation_i64 = generation_i64(generation)?;
        let id_value = id.as_str();
        let operation = sqlx::query!(
            r#"SELECT id AS "id!",state AS "state!" FROM operations WHERE application_id=?1 AND generation=?2 AND kind='replace' ORDER BY created_at_ms DESC,id DESC LIMIT 1"#,
            id_value,
            generation_i64
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::Corrupt)?;
        if !matches!(operation.state.as_str(), "failed" | "cancelled") {
            return Err(StoreError::IllegalTransition);
        }
        sqlx::query!(
            "UPDATE operations SET state='pending',error_code=NULL,error_message=NULL,updated_at_ms=?1,started_at_ms=NULL,finished_at_ms=NULL WHERE id=?2 AND state IN ('failed','cancelled')",
            now,
            operation.id
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        sqlx::query!(
            "DELETE FROM operation_steps WHERE operation_id=?1",
            operation.id
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Self::insert_operation_steps(tx, &operation.id, steps, now).await?;
        Self::set_application_status(tx, id_value, "pending", now).await?;
        Ok(MutationResult {
            generation,
            operation_id: operation.id,
        })
    }
}
