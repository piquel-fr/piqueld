//! Application target persistence and atomic operation creation.

use super::{
    ApplicationId, ApplicationPage, ApplicationRow, ApplicationStatus, NormalizedApplication,
    Operation, OperationKind, ResolvedApplication, SqliteStore, StoreError, StoredApplication,
    now_ms, page_limit,
};

impl SqliteStore {
    /// Stores a resolved target and replaces the application's active operation.
    /// The application service decides whether this target differs from the current one.
    ///
    /// # Errors
    /// Returns a storage error or a live-name collision.
    pub async fn save_application(
        &self,
        app: &NormalizedApplication,
        resolved: &ResolvedApplication,
    ) -> Result<Operation, StoreError> {
        let desired = serde_json::to_string(app).map_err(StoreError::corrupt)?;
        let resolved = serde_json::to_string(resolved).map_err(StoreError::corrupt)?;
        let id = app.id.as_str();
        let name = app.metadata.name.as_str();
        let now = now_ms();
        let mut tx = self.begin_immediate().await?;
        sqlx::query!("INSERT INTO applications(id,name,desired_json,resolved_json,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?5) ON CONFLICT(id) DO UPDATE SET name=excluded.name,desired_json=excluded.desired_json,resolved_json=excluded.resolved_json,delete_intent=0,deleted_at_ms=NULL,updated_at_ms=excluded.updated_at_ms",id,name,desired,resolved,now)
            .execute(&mut *tx).await.map_err(|error| {
                if error.as_database_error().is_some_and(sqlx::error::DatabaseError::is_unique_violation) { StoreError::AlreadyExists } else { StoreError::database(error) }
            })?;
        let operation = Self::insert_operation(&mut tx, &app.id, OperationKind::Apply, now).await?;
        Self::write_status(&mut tx, id, "pending", None, now).await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(operation)
    }

    /// Stores deletion intent and cancels unfinished apply work in the same transaction.
    ///
    /// # Errors
    /// Returns a storage error or `NotFound` for an absent application.
    pub async fn request_delete(&self, id: &ApplicationId) -> Result<Operation, StoreError> {
        let mut tx = self.begin_immediate().await?;
        let now = now_ms();
        let app_id = id.as_str();
        let changed = sqlx::query!("UPDATE applications SET delete_intent=1,updated_at_ms=?1 WHERE id=?2 AND deleted_at_ms IS NULL",now,app_id)
            .execute(&mut *tx).await.map_err(StoreError::database)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::NotFound);
        }
        let operation = Self::insert_operation(&mut tx, id, OperationKind::Delete, now).await?;
        Self::write_status(&mut tx, app_id, "deleting", None, now).await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(operation)
    }

    /// Reopens the latest successful operation when the controller observes drift.
    /// Returns `None` if a newer request arrived during observation.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn request_reconcile(
        &self,
        id: &ApplicationId,
        previous_operation_id: &str,
    ) -> Result<Option<Operation>, StoreError> {
        let mut tx = self.begin_immediate().await?;
        let now = now_ms();
        let app_id = id.as_str();
        let changed = sqlx::query!("UPDATE operations SET state='requested',error_code=NULL,error_message=NULL,started_at_ms=NULL,finished_at_ms=NULL,updated_at_ms=?1 WHERE id=?2 AND application_id=?3 AND kind='apply' AND state='succeeded' AND id=(SELECT latest.id FROM operations latest WHERE latest.application_id=?3 ORDER BY latest.created_at_ms DESC,latest.id DESC LIMIT 1)",now,previous_operation_id,app_id)
            .execute(&mut *tx).await.map_err(StoreError::database)?.rows_affected();
        if changed == 0 {
            return Ok(None);
        }
        Self::write_status(&mut tx, app_id, "pending", None, now).await?;
        tx.commit().await.map_err(StoreError::database)?;
        self.operation(previous_operation_id).await.map(Some)
    }

    /// Reads a live application.
    ///
    /// # Errors
    /// Returns a storage error or `NotFound`.
    pub async fn get(&self, id: &ApplicationId) -> Result<StoredApplication, StoreError> {
        let id = id.as_str();
        sqlx::query_as!(ApplicationRow,
            r#"SELECT id AS "id!",desired_json,resolved_json,delete_intent,created_at_ms,updated_at_ms FROM applications WHERE id=?1 AND deleted_at_ms IS NULL"#,id)
            .fetch_optional(&self.pool).await.map_err(StoreError::database)?
            .ok_or(StoreError::NotFound)?.decode()
    }

    /// Reads desired state and status from one database snapshot.
    ///
    /// # Errors
    /// Returns a storage error or `NotFound`.
    pub async fn get_with_status(
        &self,
        id: &ApplicationId,
    ) -> Result<(StoredApplication, ApplicationStatus), StoreError> {
        let mut tx = self.pool.begin().await.map_err(StoreError::database)?;
        let app_id = id.as_str();
        let application = sqlx::query_as!(ApplicationRow,
            r#"SELECT id AS "id!",desired_json,resolved_json,delete_intent,created_at_ms,updated_at_ms FROM applications WHERE id=?1 AND deleted_at_ms IS NULL"#,app_id)
            .fetch_optional(&mut *tx).await.map_err(StoreError::database)?
            .ok_or(StoreError::NotFound)?.decode()?;
        let status = Self::status_on(&mut tx, id).await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok((application, status))
    }

    /// Finds a live application by manifest name, including applications being deleted.
    ///
    /// # Errors
    /// Returns a storage or decoding error.
    pub async fn find_by_name(&self, name: &str) -> Result<Option<StoredApplication>, StoreError> {
        sqlx::query_as!(ApplicationRow,
            r#"SELECT id AS "id!",desired_json,resolved_json,delete_intent,created_at_ms,updated_at_ms FROM applications WHERE name=?1 AND deleted_at_ms IS NULL"#,name)
            .fetch_optional(&self.pool).await.map_err(StoreError::database)?
            .map(ApplicationRow::decode).transpose()
    }

    /// Lists live applications by ID. Corrupt rows are logged and skipped.
    ///
    /// # Errors
    /// Returns a storage error or an invalid pagination error.
    pub async fn list(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<ApplicationPage, StoreError> {
        let fetch_limit = page_limit(limit)? + 1;
        let after = cursor
            .map(|value| {
                value
                    .strip_prefix("v1:")
                    .ok_or(StoreError::InvalidInput)
                    .and_then(|id| ApplicationId::parse(id).map_err(StoreError::invalid_input))
            })
            .transpose()?;
        let after = after.as_ref().map_or("", ApplicationId::as_str);
        let mut rows = sqlx::query_as!(ApplicationRow,
            r#"SELECT id AS "id!",desired_json,resolved_json,delete_intent,created_at_ms,updated_at_ms FROM applications WHERE id>?1 AND deleted_at_ms IS NULL ORDER BY id LIMIT ?2"#,after,fetch_limit)
            .fetch_all(&self.pool).await.map_err(StoreError::database)?;
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = if has_more {
            rows.last().map(|row| format!("v1:{}", row.id))
        } else {
            None
        };
        let items = rows.into_iter().filter_map(|row| {
            let id = row.id.clone();
            match row.decode() {
                Ok(application) => Some(application),
                Err(error) => { tracing::error!(application_id=%id,%error,"quarantined undecodable application row"); None }
            }
        }).collect();
        Ok(ApplicationPage { items, next_cursor })
    }
}
