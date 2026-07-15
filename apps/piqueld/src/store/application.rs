//! Application repository implementation.

use super::{
    ApplicationId, ApplicationRepository, ApplicationRow, MutationResult, NormalizedApplication,
    OperationKind, ResolvedApplication, SqliteStore, StoreError, StoredApplication, async_trait,
    canonical_resolved, now_ms,
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
        let stored = row.decode(&self.instance_id)?;
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
            .map(|row| row.decode(&self.instance_id))
            .collect()
    }
}
