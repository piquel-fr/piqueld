//! Application targets and atomic intent revision checks.
use super::{
    ApplicationId, ApplicationPage, ApplicationRow, ApplicationStatus, ApplicationSummaryPage,
    ApplicationSummaryRow, NormalizedApplication, Operation, OperationKind, ResolvedApplication,
    SqliteStore, StoreError, StoredApplication, now_ms, page_limit,
};
use sqlx::{Sqlite, Transaction};

impl SqliteStore {
    fn application_cursor(cursor: Option<&str>) -> Result<Option<ApplicationId>, StoreError> {
        cursor
            .map(|value| {
                value
                    .strip_prefix("v1:")
                    .ok_or(StoreError::InvalidInput)
                    .and_then(|id| ApplicationId::parse(id).map_err(StoreError::invalid_input))
            })
            .transpose()
    }

    /// Checks a caller's optional revision precondition.
    /// # Errors
    /// Returns a generation conflict for stale intent or invalid input for an oversized revision.
    pub fn check_generation(expected: Option<u64>, actual: u64) -> Result<(), StoreError> {
        if let Some(expected) = expected {
            i64::try_from(expected).map_err(StoreError::invalid_input)?;
            if expected != actual {
                return Err(StoreError::GenerationConflict { expected, actual });
            }
        }
        Ok(())
    }

    async fn generation_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: &str,
        expected: Option<u64>,
    ) -> Result<i64, StoreError> {
        let actual = sqlx::query_scalar!(
            "SELECT generation FROM applications WHERE id=?1 AND deleted_at_ms IS NULL",
            id
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)?
        .unwrap_or(0);
        Self::check_generation(
            expected,
            u64::try_from(actual).map_err(StoreError::corrupt)?,
        )?;
        Ok(actual)
    }

    /// Stores changed intent, preserving the previous resolved deployment during preparation.
    /// Optional resolved state supports importing an already prepared target.
    /// # Errors
    /// Returns storage, revision, or name collision errors.
    pub async fn save_application(
        &self,
        app: &NormalizedApplication,
        resolved: Option<&ResolvedApplication>,
        expected: Option<u64>,
    ) -> Result<Operation, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let result = Self::save_application_on(&mut tx, app, resolved, expected).await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(result)
    }

    pub(crate) async fn save_application_on(
        tx: &mut Transaction<'_, Sqlite>,
        app: &NormalizedApplication,
        resolved: Option<&ResolvedApplication>,
        expected: Option<u64>,
    ) -> Result<Operation, StoreError> {
        let id = app.id.as_str();
        let generation = Self::generation_on(tx, id, expected)
            .await?
            .checked_add(1)
            .ok_or(StoreError::InvalidInput)?;
        let desired = serde_json::to_string(app).map_err(StoreError::corrupt)?;
        let resolved = resolved
            .map(serde_json::to_string)
            .transpose()
            .map_err(StoreError::corrupt)?;
        let resolved_generation = resolved.as_ref().map(|_| generation);
        let name = app.metadata.name.as_str();
        let now = now_ms();
        sqlx::query!("INSERT INTO applications(id,name,desired_json,resolved_json,generation,resolved_generation,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?7) ON CONFLICT(id) DO UPDATE SET desired_json=excluded.desired_json,generation=excluded.generation,resolved_json=COALESCE(excluded.resolved_json,applications.resolved_json),resolved_generation=COALESCE(excluded.resolved_generation,applications.resolved_generation),delete_intent=0,updated_at_ms=excluded.updated_at_ms",id,name,desired,resolved,generation,resolved_generation,now)
            .execute(&mut **tx).await.map_err(|error| if error.as_database_error().is_some_and(sqlx::error::DatabaseError::is_unique_violation) { StoreError::AlreadyExists } else { StoreError::database(error) })?;
        let operation = Self::insert_operation(tx, &app.id, OperationKind::Apply, now).await?;
        sqlx::query!(
            "UPDATE operations SET target_json=?1 WHERE id=?2",
            resolved,
            operation.id
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Self::write_status(tx, id, "pending", None, now).await?;
        Ok(operation)
    }

    /// Persists deletion intent and its generation atomically.
    /// # Errors
    /// Returns storage, absence, or revision errors.
    pub async fn request_delete(
        &self,
        id: &ApplicationId,
        expected: Option<u64>,
    ) -> Result<Operation, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let result = Self::request_delete_on(&mut tx, id, expected).await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(result)
    }

    pub(crate) async fn request_delete_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: &ApplicationId,
        expected: Option<u64>,
    ) -> Result<Operation, StoreError> {
        let app_id = id.as_str();
        Self::generation_on(tx, app_id, expected).await?;
        let now = now_ms();
        let changed = sqlx::query!("UPDATE applications SET delete_intent=1,generation=generation+1,updated_at_ms=?1 WHERE id=?2 AND deleted_at_ms IS NULL AND delete_intent=0",now,app_id).execute(&mut **tx).await.map_err(StoreError::database)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::IllegalTransition);
        }
        let operation = Self::insert_operation(tx, id, OperationKind::Delete, now).await?;
        Self::write_status(tx, app_id, "deleting", None, now).await?;
        Ok(operation)
    }

    /// Starts image refresh without changing the manifest generation.
    /// # Errors
    /// Returns storage, deletion-intent, or revision errors.
    pub async fn request_refresh(
        &self,
        id: &ApplicationId,
        expected: Option<u64>,
    ) -> Result<Operation, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let result = Self::request_refresh_on(&mut tx, id, expected).await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(result)
    }

    pub(crate) async fn request_refresh_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: &ApplicationId,
        expected: Option<u64>,
    ) -> Result<Operation, StoreError> {
        let app_id = id.as_str();
        Self::generation_on(tx, app_id, expected).await?;
        let deleting = sqlx::query_scalar!(
            "SELECT delete_intent FROM applications WHERE id=?1 AND deleted_at_ms IS NULL",
            app_id
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        if deleting != 0 {
            return Err(StoreError::IllegalTransition);
        }
        let now = now_ms();
        let operation = Self::insert_operation(tx, id, OperationKind::Refresh, now).await?;
        Self::write_status(tx, app_id, "pending", None, now).await?;
        Ok(operation)
    }

    /// Reopens the latest terminal operation against its own prepared target.
    /// # Errors
    /// Returns a storage error; stale observations return None.
    pub async fn request_reconcile(
        &self,
        id: &ApplicationId,
        previous_operation_id: &str,
    ) -> Result<Option<Operation>, StoreError> {
        let operation = self.operation(previous_operation_id).await?;
        if operation.application_id != *id || !operation.state.terminal() {
            return Ok(None);
        }
        match self.retry_operation(&operation).await {
            Ok(operation) => Ok(Some(operation)),
            Err(StoreError::IllegalTransition) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Reads an operation's immutable prepared target, if preparation completed.
    /// # Errors
    /// Returns a storage or decoding error, or `NotFound` for a missing operation.
    pub async fn prepared_target(
        &self,
        id: &str,
    ) -> Result<Option<ResolvedApplication>, StoreError> {
        let json = sqlx::query_scalar!("SELECT target_json FROM operations WHERE id=?1", id)
            .fetch_optional(&self.pool)
            .await
            .map_err(StoreError::database)?
            .ok_or(StoreError::NotFound)?;
        json.as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(StoreError::corrupt)
    }

    /// Publishes a completely resolved target only while its operation is current.
    /// # Errors
    /// Returns storage errors or `IllegalTransition` for obsolete preparation.
    pub async fn save_prepared(
        &self,
        operation: &Operation,
        resolved: &ResolvedApplication,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let json = serde_json::to_string(resolved).map_err(StoreError::corrupt)?;
        let changed=sqlx::query!("UPDATE operations SET target_json=?1 WHERE id=?2 AND state='running' AND id=(SELECT latest.id FROM operations latest WHERE latest.application_id=operations.application_id ORDER BY latest.created_at_ms DESC,latest.id DESC LIMIT 1)",json,operation.id).execute(&mut *tx).await.map_err(StoreError::database)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::IllegalTransition);
        }
        Self::operation_event(&mut tx, &operation.id, "target_resolved", None, now_ms()).await?;
        tx.commit().await.map_err(StoreError::database)
    }

    /// Publishes the prepared target after ownership and configuration checks pass.
    /// # Errors
    /// Returns a store error or `IllegalTransition` for obsolete work.
    pub async fn publish_prepared(&self, operation: &Operation) -> Result<(), StoreError> {
        let app_id = operation.application_id.as_str();
        let (_writer, mut tx) = self.begin_immediate().await?;
        let changed=sqlx::query!("UPDATE applications SET resolved_json=(SELECT target_json FROM operations WHERE id=?1),resolved_generation=generation WHERE id=?2 AND ?1=(SELECT latest.id FROM operations latest WHERE latest.application_id=applications.id ORDER BY latest.created_at_ms DESC,latest.id DESC LIMIT 1) AND EXISTS(SELECT 1 FROM operations WHERE id=?1 AND state='running' AND target_json IS NOT NULL)",operation.id,app_id).execute(&mut *tx).await.map_err(StoreError::database)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::IllegalTransition);
        }
        let promoted = sqlx::query!(
            "UPDATE operations SET promoted=1 WHERE id=?1 AND promoted=0",
            operation.id
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        if promoted == 1 {
            Self::operation_event(&mut tx, &operation.id, "target_promoted", None, now_ms())
                .await?;
        }
        tx.commit().await.map_err(StoreError::database)
    }

    /// Reads a live application.
    ///
    /// # Errors
    /// Returns a storage error or `NotFound`.
    pub async fn get(&self, id: &ApplicationId) -> Result<StoredApplication, StoreError> {
        let id = id.as_str();
        sqlx::query_as!(ApplicationRow,
            r#"SELECT id AS "id!",desired_json,resolved_json,generation,resolved_generation,delete_intent,created_at_ms,updated_at_ms FROM applications WHERE id=?1 AND deleted_at_ms IS NULL"#,id)
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
            r#"SELECT id AS "id!",desired_json,resolved_json,generation,resolved_generation,delete_intent,created_at_ms,updated_at_ms FROM applications WHERE id=?1 AND deleted_at_ms IS NULL"#,app_id)
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
            r#"SELECT id AS "id!",desired_json,resolved_json,generation,resolved_generation,delete_intent,created_at_ms,updated_at_ms FROM applications WHERE name=?1 AND deleted_at_ms IS NULL"#,name)
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
        let after = Self::application_cursor(cursor)?;
        let after = after.as_ref().map_or("", ApplicationId::as_str);
        let mut rows = sqlx::query_as!(ApplicationRow,
            r#"SELECT id AS "id!",desired_json,resolved_json,generation,resolved_generation,delete_intent,created_at_ms,updated_at_ms FROM applications WHERE id>?1 AND deleted_at_ms IS NULL ORDER BY id LIMIT ?2"#,after,fetch_limit)
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

    /// Lists live application metadata by ID without reading manifest documents.
    ///
    /// # Errors
    /// Returns a storage error or an invalid pagination error.
    pub async fn list_summaries(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<ApplicationSummaryPage, StoreError> {
        let fetch_limit = page_limit(limit)? + 1;
        let after = Self::application_cursor(cursor)?;
        let after = after.as_ref().map_or("", ApplicationId::as_str);
        let mut rows = sqlx::query_as!(ApplicationSummaryRow,
            r#"SELECT id AS "id!",name AS "name!",generation,resolved_generation,delete_intent,created_at_ms,updated_at_ms FROM applications WHERE id>?1 AND deleted_at_ms IS NULL ORDER BY id LIMIT ?2"#,after,fetch_limit)
            .fetch_all(&self.pool).await.map_err(StoreError::database)?;
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = if has_more {
            rows.last().map(|row| format!("v1:{}", row.id))
        } else {
            None
        };
        let items = rows
            .into_iter()
            .map(ApplicationSummaryRow::decode)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ApplicationSummaryPage { items, next_cursor })
    }
}
