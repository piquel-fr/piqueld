//! Application repository implementation.

use super::{
    ApplicationId, ApplicationPage, ApplicationRow, MutationResult, NormalizedApplication,
    OperationKind, ResolvedApplication, Sqlite, SqliteStore, StoreError, StoredApplication,
    Transaction, canonical_resolved, generation_i64, now_ms, page_limit, valid_sha256,
    validate_operation_steps,
};
use uuid::Uuid;

impl SqliteStore {
    /// Creates an application at generation 1, initializes its pending status, and records a durable create operation.
    ///
    /// # Parameters
    ///
    /// * `steps` — The steps associated with the create operation.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::AlreadyExists`] when an application with the same identifier exists. Returns other
    /// [`StoreError`] variants when application data cannot be canonicalized or the transaction cannot be persisted.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let result = store.create(&app, &resolved, &[]).await?;
    /// assert_eq!(result.generation, 1);
    /// # Ok::<(), StoreError>(())
    /// ```
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
            "INSERT OR IGNORE INTO applications(id,name,generation,desired_json,resolved_json,spec_hash,created_at_ms,updated_at_ms) VALUES(?1,?2,1,?3,?4,?5,?6,?6)",
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

    /// Creates an application and associates the request with an idempotency key.
    ///
    /// If the key is already bound to the same request and application, returns the
    /// previously created mutation result. A conflicting binding is rejected.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidInput`] for invalid SHA-256 hashes,
    /// [`StoreError::IdempotencyConflict`] for a conflicting binding,
    /// [`StoreError::AlreadyExists`] if the application already exists, or another
    /// [`StoreError`] variant when persisted data is corrupt or the transaction
    /// cannot be completed.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(
    /// #     store: &SqliteStore,
    /// #     app: &NormalizedApplication,
    /// #     resolved: &ResolvedApplication,
    /// # ) -> Result<(), StoreError> {
    /// let result = store
    ///     .create_idempotent(
    ///         app,
    ///         resolved,
    ///         &[],
    ///         "0000000000000000000000000000000000000000000000000000000000000000",
    ///         "1111111111111111111111111111111111111111111111111111111111111111",
    ///     )
    ///     .await?;
    ///
    /// assert_eq!(result.generation, 1);
    /// # Ok(())
    /// # }
    /// ```
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
            r#"SELECT request_hash AS "request_hash!",application_id AS "application_id!",operation_id AS "operation_id!",generation AS "generation!" FROM application_create_idempotency WHERE key_hash=?1"#,
            key_hash
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        if let Some(row) = existing {
            if row.request_hash != request_hash || row.application_id != app.id.as_str() {
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
            "INSERT OR IGNORE INTO applications(id,name,generation,desired_json,resolved_json,spec_hash,created_at_ms,updated_at_ms) VALUES(?1,?2,1,?3,?4,?5,?6,?6)",
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
            "INSERT INTO application_create_idempotency(key_hash,request_hash,application_id,operation_id,generation,created_at_ms) VALUES(?1,?2,?3,?4,1,?5)",
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

    /// Looks up a create request by its hashed idempotency key.
    ///
    /// Returns the previously recorded mutation result when the key matches the
    /// application and request hashes, or `None` when no binding exists.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidInput`] for invalid SHA-256 hashes,
    /// [`StoreError::IdempotencyConflict`] when the binding does not match the
    /// application or request, [`StoreError::Corrupt`] for an invalid generation,
    /// or [`StoreError`] for database failures.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// let result = store
    ///     .create_idempotency(&app_id, &key_hash, &request_hash)
    ///     .await?;
    /// # Ok::<(), StoreError>(())
    /// ```
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
            r#"SELECT request_hash AS "request_hash!",application_id AS "application_id!",operation_id AS "operation_id!",generation AS "generation!" FROM application_create_idempotency WHERE key_hash=?1"#,
            key_hash
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?;
        let Some(row) = row else {
            return Ok(None);
        };
        if row.request_hash != request_hash || row.application_id != app_id.as_str() {
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

    /// Replaces an application's desired and resolved data, advances its generation, and records a replacement operation.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::GenerationConflict`] when the expected generation is stale,
    /// [`StoreError::IllegalTransition`] when deletion is in progress, or
    /// [`StoreError::AlreadyExists`] when the application has changed without advancing
    /// its generation. It may also return errors for missing or corrupt persisted data,
    /// generation overflow, database failures, or transaction commit failures.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// let result = store.replace(&application, &resolved, 1, &steps).await?;
    /// assert_eq!(result.generation, 2);
    /// # Ok::<(), StoreError>(())
    /// ```
    pub async fn replace(
        &self,
        app: &NormalizedApplication,
        resolved: &ResolvedApplication,
        expected_generation: u64,
        steps: &[String],
    ) -> Result<MutationResult, StoreError> {
        let mut tx = self.begin_immediate().await?;
        let generation = expected_generation
            .checked_add(1)
            .ok_or(StoreError::Corrupt)?;
        let desired = app.canonical_json().map_err(StoreError::corrupt)?;
        let resolved = canonical_resolved(app, resolved, &self.instance_id)?;
        let hash = app.spec_hash();
        let now = now_ms();
        let app_id = app.id.as_str();
        let app_name = app.metadata.name.as_str();
        let new_generation = generation_i64(generation)?;
        let expected_generation_db = generation_i64(expected_generation)?;
        let changed = sqlx::query!(
            "UPDATE OR IGNORE applications SET name=?1,generation=?2,desired_json=?3,resolved_json=?4,spec_hash=?5,updated_at_ms=?6 WHERE id=?7 AND generation=?8 AND delete_intent=0",
            app_name,
            new_generation,
            desired,
            resolved,
            hash,
            now,
            app_id,
            expected_generation_db
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        if changed != 1 {
            let row = sqlx::query!(
                "SELECT generation,delete_intent FROM applications WHERE id=?1",
                app_id
            )
            .fetch_optional(&mut *tx)
            .await
            .map_err(StoreError::database)?
            .ok_or(StoreError::NotFound)?;
            let actual = u64::try_from(row.generation).map_err(StoreError::corrupt)?;
            return if actual == expected_generation && row.delete_intent == 1 {
                Err(StoreError::IllegalTransition)
            } else if actual == expected_generation {
                Err(StoreError::AlreadyExists)
            } else {
                Err(StoreError::GenerationConflict {
                    expected: expected_generation,
                    actual,
                })
            };
        }
        let status_changed = sqlx::query!(
            "UPDATE application_status SET state='pending',observed_generation=NULL,message=NULL,updated_at_ms=?1 WHERE application_id=?2",
            now,
            app_id
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        if status_changed != 1 {
            return Err(StoreError::Corrupt);
        }
        let operation_id = Self::insert_operation(
            &mut tx,
            &app.id,
            generation,
            OperationKind::Replace,
            steps,
            now,
        )
        .await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(MutationResult {
            generation,
            operation_id,
        })
    }

    /// Queues a reconciliation operation for the specified application generation.
    ///
    /// An existing pending, running, or recovery reconciliation operation for the
    /// generation is reused. Otherwise, a new operation is created and the
    /// application status is reset to pending.
    ///
    /// # Parameters
    ///
    /// * `expected_generation` — The application generation to reconcile.
    /// * `steps` — The reconciliation steps to associate with the operation.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] when the application does not exist,
    /// [`StoreError::GenerationConflict`] when the expected generation is stale,
    /// [`StoreError::IllegalTransition`] when deletion is in progress, or a
    /// persistence error when the operation cannot be durably stored.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(
    /// #     store: &SqliteStore,
    /// #     id: &ApplicationId,
    /// # ) -> Result<(), StoreError> {
    /// let result = store.request_reconcile(id, 1, &[]).await?;
    /// assert_eq!(result.generation, 1);
    /// # Ok(())
    /// # }
    /// ```
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

    /// Finds the active reconciliation operation for an application generation.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// let result = store.active_reconcile(&application_id, 1).await?;
    /// # Ok::<(), StoreError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the generation cannot be represented or the journal
    /// cannot be read.
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

    /// Requests deletion of an application and records the corresponding delete operation.
    ///
    /// An existing active delete operation is reused. Failed or cancelled delete operations
    /// are reset with the supplied steps.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// let result = store.request_delete(&id, generation, &steps).await?;
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] if the application does not exist, or
    /// [`StoreError::GenerationConflict`] if its generation differs from
    /// `expected_generation`. Also returns lifecycle, corruption, and database errors.
    ///
    /// Errors are returned if the generation cannot be incremented or a durable write fails.
    pub async fn request_delete(
        &self,
        id: &ApplicationId,
        expected_generation: u64,
        steps: &[String],
    ) -> Result<MutationResult, StoreError> {
        let mut tx = self.begin_immediate().await?;
        let now = now_ms();
        let id_value = id.as_str();
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
            tx.commit().await.map_err(StoreError::database)?;
            return Ok(result);
        }
        let generation = expected_generation
            .checked_add(1)
            .ok_or(StoreError::Corrupt)?;
        let generation_i64 = generation_i64(generation)?;
        let changed = sqlx::query!(
            "UPDATE applications SET generation=?1,delete_intent=1,updated_at_ms=?2 WHERE id=?3 AND generation=?4 AND delete_intent=0",
            generation_i64,
            now,
            id_value,
            expected_generation_i64
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        if changed != 1 {
            let row = sqlx::query!(
                "SELECT generation,delete_intent FROM applications WHERE id=?1",
                id_value
            )
            .fetch_optional(&mut *tx)
            .await
            .map_err(StoreError::database)?
            .ok_or(StoreError::NotFound)?;
            let actual = u64::try_from(row.generation).map_err(StoreError::corrupt)?;
            return if actual == expected_generation && row.delete_intent == 1 {
                Err(StoreError::IllegalTransition)
            } else {
                Err(StoreError::GenerationConflict {
                    expected: expected_generation,
                    actual,
                })
            };
        }
        let status_changed = sqlx::query!(
            "UPDATE application_status SET state='deleting',observed_generation=NULL,message=NULL,updated_at_ms=?1 WHERE application_id=?2",
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
        let operation_id =
            Self::insert_operation(&mut tx, id, generation, OperationKind::Delete, steps, now)
                .await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(MutationResult {
            generation,
            operation_id,
        })
    }

    /// Finalizes a succeeded delete operation by tombstoning the application.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the generation is invalid, the delete operation has
    /// not succeeded, or the tombstone cannot be persisted.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(
    /// #     store: &SqliteStore,
    /// #     id: &ApplicationId,
    /// # ) -> Result<(), StoreError> {
    /// store.finalize_delete(id, 1).await?;
    /// # Ok(())
    /// # }
    /// ```
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

    /// Finalizes a successfully completed delete operation by tombstoning the application.
    ///
    /// The application must have a delete operation in the succeeded state for the specified
    /// generation and must still be marked for deletion. A unique tombstone name is assigned.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(
    /// #     store: &SqliteStore,
    /// #     id: &ApplicationId,
    /// # ) -> Result<(), StoreError> {
    /// store.finalize_delete(id, 7).await?;
    /// # Ok(())
    /// # }
    /// ```
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
            Ok(())
        } else {
            Err(StoreError::IllegalTransition)
        }
    }

    /// Reads a live application and validates its persisted representation.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] when the application is absent or tombstoned.
    /// Returns [`StoreError::Corrupt`] when the persisted application ID does not match
    /// the requested ID or its data fails validation. Database failures are returned as
    /// store errors.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(
    /// #     store: &SqliteStore,
    /// #     id: &ApplicationId,
    /// # ) -> Result<(), StoreError> {
    /// let application = store.get(id).await?;
    /// # let _ = application;
    /// # Ok(())
    /// # }
    /// ```
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

    /// Finds a live application by its user-facing name.
    ///
    /// Returns `None` when no live application has the specified name.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the database query fails or the stored application
    /// data cannot be decoded or validated.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(store: &SqliteStore) -> Result<(), StoreError> {
    /// let application = store.find_by_name("example").await?;
    /// assert!(application.is_some());
    /// # Ok(())
    /// # }
    /// ```
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
    /// An optional versioned cursor continues the listing after the application
    /// identified by the cursor.
    ///
    /// # Arguments
    ///
    /// * `cursor` - An optional cursor returned by a previous page.
    /// * `limit` - The maximum number of applications to return.
    ///
    /// # Returns
    ///
    /// A page of applications and an optional cursor for the next page.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if pagination is invalid, the cursor is malformed,
    /// the database query fails, or persisted application data cannot be decoded
    /// and validated.
    ///
    /// # Panics
    ///
    /// Panics if a non-empty bounded page cannot provide a cursor, indicating a
    /// database or query invariant violation.
    ///
    /// # Examples
    ///
    /// ```
    /// # async fn example(store: &SqliteStore) -> Result<(), StoreError> {
    /// let page = store.list(None, 20).await?;
    /// println!("{} applications", page.items.len());
    /// # Ok(())
    /// # }
    /// ```
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
        let mut items = rows
            .into_iter()
            .map(|row| row.decode(&self.instance_id))
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = items.len() > limit;
        items.truncate(limit);
        let next_cursor = has_more.then(|| {
            format!(
                "v1:{}",
                items
                    .last()
                    .expect("a non-empty bounded page has a cursor")
                    .application
                    .id
            )
        });
        Ok(ApplicationPage { items, next_cursor })
    }
}

impl SqliteStore {
    /// Resets a failed or cancelled delete operation for an application and replaces its steps.
    ///
    /// The application status is restored to `deleting`, and the returned result identifies
    /// the operation and its generation.
    ///
    /// # Errors
    ///
    /// Returns an error if the steps are invalid, the generation cannot be represented,
    /// the delete operation is missing or not failed or cancelled, or a database update fails.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let result = reset_failed_delete(&mut tx, &id, generation, &steps, now).await?;
    /// assert_eq!(result.generation, generation);
    /// # Ok::<(), StoreError>(())
    /// ```
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
}
