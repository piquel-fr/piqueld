//! Application repository implementation.

use super::{
    ApplicationId, ApplicationRepository, ApplicationRow, MutationResult, NormalizedApplication,
    OperationKind, ResolvedApplication, SqliteStore, StoreError, StoredApplication, async_trait,
    canonical_resolved, decode_stored_application, now_ms, valid_sha256,
};

#[async_trait]
impl ApplicationRepository for SqliteStore {
    async fn create(
        &self,
        app: &NormalizedApplication,
        resolved: Option<&ResolvedApplication>,
        steps: &[String],
    ) -> Result<MutationResult, StoreError> {
        let mut tx = self.begin_immediate().await?;
        self.validate_secrets(&mut tx, app).await?;
        let desired = app.canonical_json().map_err(|_| StoreError::Corrupt)?;
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
        .map_err(|_| StoreError::Database)?
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
        .map_err(|_| StoreError::Database)?;
        let operation_id =
            Self::insert_operation(&mut tx, &app.id, 1, OperationKind::Create, steps, now).await?;
        tx.commit().await.map_err(|_| StoreError::Database)?;
        Ok(MutationResult {
            generation: 1,
            operation_id,
        })
    }

    async fn create_idempotent(
        &self,
        app: &NormalizedApplication,
        resolved: Option<&ResolvedApplication>,
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
        .map_err(|_| StoreError::Database)?;
        if let Some(row) = existing {
            if row.request_hash != request_hash || row.application_id != app.id.as_str() {
                return Err(StoreError::IdempotencyConflict);
            }
            let generation = u64::try_from(row.generation).map_err(|_| StoreError::Corrupt)?;
            if generation != 1 {
                return Err(StoreError::Corrupt);
            }
            tx.commit().await.map_err(|_| StoreError::Database)?;
            return Ok(MutationResult {
                generation,
                operation_id: row.operation_id,
            });
        }

        self.validate_secrets(&mut tx, app).await?;
        let desired = app.canonical_json().map_err(|_| StoreError::Corrupt)?;
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
        .map_err(|_| StoreError::Database)?
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
        .map_err(|_| StoreError::Database)?;
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
        .map_err(|_| StoreError::Database)?;
        tx.commit().await.map_err(|_| StoreError::Database)?;
        Ok(MutationResult {
            generation: 1,
            operation_id,
        })
    }

    async fn create_idempotency(
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
        .map_err(|_| StoreError::Database)?;
        let Some(row) = row else {
            return Ok(None);
        };
        if row.request_hash != request_hash || row.application_id != app_id.as_str() {
            return Err(StoreError::IdempotencyConflict);
        }
        let generation = u64::try_from(row.generation).map_err(|_| StoreError::Corrupt)?;
        if generation != 1 {
            return Err(StoreError::Corrupt);
        }
        Ok(Some(MutationResult {
            generation,
            operation_id: row.operation_id,
        }))
    }

    async fn replace(
        &self,
        app: &NormalizedApplication,
        resolved: Option<&ResolvedApplication>,
        expected_generation: u64,
        steps: &[String],
    ) -> Result<MutationResult, StoreError> {
        let mut tx = self.begin_immediate().await?;
        self.validate_secrets(&mut tx, app).await?;
        let generation = expected_generation
            .checked_add(1)
            .ok_or(StoreError::Corrupt)?;
        let desired = app.canonical_json().map_err(|_| StoreError::Corrupt)?;
        let resolved = canonical_resolved(app, resolved, &self.instance_id)?;
        let hash = app.spec_hash();
        let now = now_ms();
        let app_id = app.id.as_str();
        let app_name = app.metadata.name.as_str();
        let generation_i64 = generation as i64;
        let expected_generation_i64 = expected_generation as i64;
        let changed = sqlx::query!(
            "UPDATE OR IGNORE applications SET name=?1,generation=?2,desired_json=?3,resolved_json=?4,spec_hash=?5,updated_at_ms=?6 WHERE id=?7 AND generation=?8 AND delete_intent=0",
            app_name,
            generation_i64,
            desired,
            resolved,
            hash,
            now,
            app_id,
            expected_generation_i64
        )
        .execute(&mut *tx)
        .await
        .map_err(|_| StoreError::Database)?
        .rows_affected();
        if changed != 1 {
            let row = sqlx::query!(
                "SELECT generation,delete_intent FROM applications WHERE id=?1",
                app_id
            )
            .fetch_optional(&mut *tx)
            .await
            .map_err(|_| StoreError::Database)?
            .ok_or(StoreError::NotFound)?;
            let actual = u64::try_from(row.generation).map_err(|_| StoreError::Corrupt)?;
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
        .map_err(|_| StoreError::Database)?
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
        tx.commit().await.map_err(|_| StoreError::Database)?;
        Ok(MutationResult {
            generation,
            operation_id,
        })
    }

    async fn request_reconcile(
        &self,
        id: &ApplicationId,
        expected_generation: u64,
        steps: &[String],
    ) -> Result<MutationResult, StoreError> {
        let mut tx = self.begin_immediate().await?;
        let id_value = id.as_str();
        let expected_generation_i64 = expected_generation as i64;
        let row = sqlx::query!(
            "SELECT generation,delete_intent FROM applications WHERE id=?1",
            id_value
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| StoreError::Database)?
        .ok_or(StoreError::NotFound)?;
        let actual = u64::try_from(row.generation).map_err(|_| StoreError::Corrupt)?;
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
        .map_err(|_| StoreError::Database)?;
        if let Some(operation_id) = existing {
            tx.commit().await.map_err(|_| StoreError::Database)?;
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
        .map_err(|_| StoreError::Database)?
        .rows_affected();
        if status_changed != 1 {
            return Err(StoreError::Corrupt);
        }
        tx.commit().await.map_err(|_| StoreError::Database)?;
        Ok(MutationResult {
            generation: expected_generation,
            operation_id,
        })
    }

    async fn active_reconcile(
        &self,
        id: &ApplicationId,
        expected_generation: u64,
    ) -> Result<Option<MutationResult>, StoreError> {
        let id_value = id.as_str();
        let expected_generation_i64 = expected_generation as i64;
        let operation_id = sqlx::query_scalar!(
            r#"SELECT id AS "id!" FROM operations WHERE application_id=?1 AND generation=?2 AND kind='reconcile' AND state IN ('pending','running','recovery') ORDER BY created_at_ms DESC,id DESC LIMIT 1"#,
            id_value,
            expected_generation_i64
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| StoreError::Database)?;
        Ok(operation_id.map(|operation_id| MutationResult {
            generation: expected_generation,
            operation_id,
        }))
    }

    async fn request_delete(
        &self,
        id: &ApplicationId,
        expected_generation: u64,
        steps: &[String],
    ) -> Result<MutationResult, StoreError> {
        let mut tx = self.begin_immediate().await?;
        let generation = expected_generation
            .checked_add(1)
            .ok_or(StoreError::Corrupt)?;
        let now = now_ms();
        let id_value = id.as_str();
        let generation_i64 = generation as i64;
        let expected_generation_i64 = expected_generation as i64;
        let changed = sqlx::query!(
            "UPDATE applications SET generation=?1,delete_intent=1,updated_at_ms=?2 WHERE id=?3 AND generation=?4 AND delete_intent=0",
            generation_i64,
            now,
            id_value,
            expected_generation_i64
        )
        .execute(&mut *tx)
        .await
        .map_err(|_| StoreError::Database)?
        .rows_affected();
        if changed != 1 {
            let row = sqlx::query!(
                "SELECT generation,delete_intent FROM applications WHERE id=?1",
                id_value
            )
            .fetch_optional(&mut *tx)
            .await
            .map_err(|_| StoreError::Database)?
            .ok_or(StoreError::NotFound)?;
            let actual = u64::try_from(row.generation).map_err(|_| StoreError::Corrupt)?;
            if actual == expected_generation && row.delete_intent == 1 {
                return Err(StoreError::IllegalTransition);
            }
            return Err(StoreError::GenerationConflict {
                expected: expected_generation,
                actual,
            });
        }
        let status_changed = sqlx::query!(
            "UPDATE application_status SET state='deleting',observed_generation=NULL,message=NULL,updated_at_ms=?1 WHERE application_id=?2",
            now,
            id_value
        )
        .execute(&mut *tx)
        .await
        .map_err(|_| StoreError::Database)?
        .rows_affected();
        if status_changed != 1 {
            return Err(StoreError::Corrupt);
        }
        let operation_id =
            Self::insert_operation(&mut tx, id, generation, OperationKind::Delete, steps, now)
                .await?;
        tx.commit().await.map_err(|_| StoreError::Database)?;
        Ok(MutationResult {
            generation,
            operation_id,
        })
    }

    async fn get(&self, id: &ApplicationId) -> Result<StoredApplication, StoreError> {
        let id_value = id.as_str();
        let row = sqlx::query_as!(
            ApplicationRow,
            r#"SELECT id AS "id!",name AS "name!",desired_json AS "desired_json!",resolved_json,generation AS "generation!",spec_hash AS "spec_hash!",delete_intent AS "delete_intent!",created_at_ms AS "created_at_ms!",updated_at_ms AS "updated_at_ms!" FROM applications WHERE id=?1"#,
            id_value
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| StoreError::Database)?
        .ok_or(StoreError::NotFound)?;
        let stored = decode_stored_application(row, &self.instance_id)?;
        if stored.application.id != *id {
            return Err(StoreError::Corrupt);
        }
        Ok(stored)
    }

    async fn list(&self) -> Result<Vec<StoredApplication>, StoreError> {
        let rows = sqlx::query_as!(
            ApplicationRow,
            r#"SELECT id AS "id!",name AS "name!",desired_json AS "desired_json!",resolved_json,generation AS "generation!",spec_hash AS "spec_hash!",delete_intent AS "delete_intent!",created_at_ms AS "created_at_ms!",updated_at_ms AS "updated_at_ms!" FROM applications ORDER BY name,id"#
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|_| StoreError::Database)?;
        rows.into_iter()
            .map(|row| decode_stored_application(row, &self.instance_id))
            .collect()
    }
}
