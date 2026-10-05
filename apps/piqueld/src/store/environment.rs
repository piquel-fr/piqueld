//! Environments: deployable units with their own operations, status, and
//! resolved targets, deploying their application's shared manifest.
use super::{
    ApplicationId, EnvironmentId, EnvironmentPage, EnvironmentStatus, Operation, OperationKind,
    ResolvedApplication, Store, StoreError, StoredEnvironment, StoredEnvironmentRow, now_ms,
    page_limit,
};
use piqueld_core::EnvironmentName;
use sqlx::{Sqlite, SqliteConnection, Transaction};

impl Store {
    /// Inserts an environment, initially `not_deployed`, and records its creation.
    /// Returns `AlreadyExists` when the application already has one with `name`.
    pub(super) async fn insert_environment_on(
        tx: &mut Transaction<'_, Sqlite>,
        application: &ApplicationId,
        id: &EnvironmentId,
        name: &EnvironmentName,
        now: i64,
    ) -> Result<(), StoreError> {
        let (application, id, name) = (application.as_str(), id.as_str(), name.as_str());
        sqlx::query!(
            "INSERT INTO environments(id,application_id,name,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?4)",
            id,
            application,
            name,
            now
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::constraint)?;
        Self::write_status(tx, id, "not_deployed", None, now).await?;
        let message = format!("created environment {name}");
        sqlx::query!(
            "INSERT INTO events(application_id,environment_id,kind,message,created_at_ms) VALUES((SELECT application_id FROM environments WHERE id=?1),?1,'environment_created',?2,?3)",
            id,
            message,
            now
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// Creates an environment of a live application under a freshly generated
    /// ID, advancing the application revision. Applications being deleted are `Busy`.
    pub(super) async fn create_environment_on(
        tx: &mut Transaction<'_, Sqlite>,
        application: &super::StoredApplication,
        name: &EnvironmentName,
        now: i64,
    ) -> Result<EnvironmentId, StoreError> {
        if application.delete_intent {
            return Err(StoreError::Busy);
        }
        let id = EnvironmentId::parse(super::new_id("env")).map_err(StoreError::corrupt)?;
        Self::insert_environment_on(tx, application.application.id(), &id, name, now).await?;
        Self::bump_generation_on(tx, application.application.id(), application.generation).await?;
        Ok(id)
    }

    /// Renames an environment that is not being deleted, advancing the
    /// application revision. Its runtime is unaffected. Names select
    /// `[spec.environments.<name>]`, so a rename is refused while the saved
    /// manifest configures the old or the new name, rather than silently
    /// switching the configuration the environment deploys.
    pub(super) async fn rename_environment_on(
        tx: &mut Transaction<'_, Sqlite>,
        environment: &StoredEnvironment,
        name: &EnvironmentName,
        now: i64,
    ) -> Result<(), StoreError> {
        if environment.delete_intent() {
            return Err(StoreError::Busy);
        }
        let old = &environment.environment.name;
        if old == name {
            return Ok(());
        }
        if let Some(configured) = [old, name]
            .into_iter()
            .find(|name| environment.manifest().configures(name))
        {
            return Err(StoreError::EnvironmentConfigured {
                environment: configured.clone(),
            });
        }
        let (id, name) = (environment.id().as_str(), name.as_str());
        sqlx::query!(
            "UPDATE environments SET name=?1,updated_at_ms=?2 WHERE id=?3",
            name,
            now,
            id
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::constraint)?;
        let message = format!("renamed environment {old} to {name}");
        sqlx::query!(
            "INSERT INTO events(application_id,environment_id,kind,message,created_at_ms) VALUES((SELECT application_id FROM environments WHERE id=?1),?1,'environment_renamed',?2,?3)",
            id,
            message,
            now
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Self::bump_generation_on(
            tx,
            environment.manifest().id(),
            environment.application.generation,
        )
        .await
    }

    /// Persists an environment's deletion intent and requests its delete operation.
    /// Its application's revision is unchanged. Returns `IllegalTransition` when
    /// deletion is already pending.
    pub(crate) async fn request_delete_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: &EnvironmentId,
    ) -> Result<Operation, StoreError> {
        let environment_id = id.as_str();
        let now = now_ms();
        let changed = sqlx::query!(
            "UPDATE environments SET delete_intent=1,updated_at_ms=?1 WHERE id=?2 AND delete_intent=0",
            now,
            environment_id
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        if changed != 1 {
            return Err(StoreError::IllegalTransition);
        }
        let operation = Self::insert_operation(tx, id, OperationKind::Delete, now).await?;
        Self::write_status(tx, environment_id, "deleting", None, now).await?;
        Ok(operation)
    }

    /// Deletes an environment without a precondition. See `request_delete_on`.
    /// # Errors
    /// Returns storage or illegal transition errors.
    pub async fn request_delete(&self, id: &EnvironmentId) -> Result<Operation, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let result = Self::request_delete_on(&mut tx, id).await?;
        Self::commit_environment_changes(tx, [id.as_str()]).await?;
        Ok(result)
    }

    /// Deploys saved configuration without changing its generation.
    /// # Errors
    /// Returns storage, deletion-intent, or revision errors.
    pub async fn request_deploy(
        &self,
        id: &EnvironmentId,
        expected: Option<u64>,
    ) -> Result<Operation, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let environment = Self::environment_on(&mut tx, id.as_str())
            .await?
            .ok_or(StoreError::NotFound)?;
        Self::check_generation(expected, environment.application.generation)?;
        let result = Self::request_deploy_on(&mut tx, &environment).await?;
        Self::commit_environment_changes(tx, [id.as_str()]).await?;
        Ok(result)
    }

    /// Creates a deployment operation for the application's saved configuration
    /// and marks the environment `pending`. Returns `IllegalTransition` while
    /// the environment is being deleted.
    pub(crate) async fn request_deploy_on(
        tx: &mut Transaction<'_, Sqlite>,
        environment: &StoredEnvironment,
    ) -> Result<Operation, StoreError> {
        if environment.delete_intent() {
            return Err(StoreError::IllegalTransition);
        }
        let now = now_ms();
        // Keep the existing on-disk operation kind for deployment records.
        let operation =
            Self::insert_operation(tx, environment.id(), OperationKind::Refresh, now).await?;
        Self::write_status(tx, environment.id().as_str(), "pending", None, now).await?;
        Ok(operation)
    }

    /// Reopens the latest terminal operation against its own prepared target.
    /// # Errors
    /// Returns a storage error; stale observations return None.
    pub async fn request_reconcile(
        &self,
        id: &EnvironmentId,
        previous_operation_id: &str,
    ) -> Result<Option<Operation>, StoreError> {
        let operation = self.operation(previous_operation_id).await?;
        if operation.environment_id != *id || !operation.state.terminal() {
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
    /// Stores the target on the running, latest operation, applies any fetched
    /// repository manifest to the application, and records `target_resolved`.
    /// # Errors
    /// Returns storage errors or `IllegalTransition` for obsolete preparation.
    pub async fn save_prepared(
        &self,
        operation: &Operation,
        resolved: &ResolvedApplication,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let json = serde_json::to_string(resolved).map_err(StoreError::corrupt)?;
        let changed=sqlx::query!("UPDATE operations SET target_json=?1 WHERE id=?2 AND state='running' AND id=(SELECT latest.id FROM operations latest WHERE latest.environment_id=operations.environment_id ORDER BY latest.created_at_ms DESC,latest.id DESC LIMIT 1)",json,operation.id).execute(&mut *tx).await.map_err(StoreError::database)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::IllegalTransition);
        }
        let applied = Self::accept_deployment_on(&mut tx, operation).await?;
        Self::operation_event(&mut tx, &operation.id, "target_resolved", None, now_ms()).await?;
        let mut changed = vec![operation.environment_id.clone()];
        // A fetched manifest is saved configuration for every environment.
        if let Some(application) = applied {
            changed = Self::environments_on(&mut tx, application.as_str())
                .await?
                .into_iter()
                .map(|environment| environment.id)
                .collect();
        }
        Self::commit_environment_changes(tx, changed.iter().map(EnvironmentId::as_str)).await
    }

    /// Publishes the prepared target after ownership and configuration checks pass.
    /// Copies the operation's target and generation onto the environment as its
    /// resolved state and marks the operation promoted, recording `target_promoted`
    /// the first time.
    /// # Errors
    /// Returns a store error or `IllegalTransition` for obsolete work.
    pub async fn publish_prepared(&self, operation: &Operation) -> Result<(), StoreError> {
        let environment_id = operation.environment_id.as_str();
        let (_writer, mut tx) = self.begin_immediate().await?;
        let changed=sqlx::query!("UPDATE environments SET resolved_json=(SELECT target_json FROM operations WHERE id=?1),resolved_generation=(SELECT generation FROM operations WHERE id=?1) WHERE id=?2 AND ?1=(SELECT latest.id FROM operations latest WHERE latest.environment_id=environments.id ORDER BY latest.created_at_ms DESC,latest.id DESC LIMIT 1) AND EXISTS(SELECT 1 FROM operations WHERE id=?1 AND state='running' AND target_json IS NOT NULL)",operation.id,environment_id).execute(&mut *tx).await.map_err(StoreError::database)?.rows_affected();
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
        Self::commit_environment_changes(tx, [environment_id]).await
    }

    /// Reads a live environment with its application's configuration.
    ///
    /// # Errors
    /// Returns a storage error or `NotFound`.
    pub async fn get(&self, id: &EnvironmentId) -> Result<StoredEnvironment, StoreError> {
        Self::environment_on(
            &mut *self.pool.acquire().await.map_err(StoreError::database)?,
            id.as_str(),
        )
        .await?
        .ok_or(StoreError::NotFound)
    }

    /// Reads an environment on an existing connection or transaction.
    pub(super) async fn environment_on(
        connection: &mut SqliteConnection,
        id: &str,
    ) -> Result<Option<StoredEnvironment>, StoreError> {
        sqlx::query_as!(StoredEnvironmentRow,
            r#"SELECT e.id AS "id!",e.application_id AS "application_id!",e.name AS "name!",e.resolved_json,e.resolved_generation,e.delete_intent AS "delete_intent!",e.created_at_ms AS "created_at_ms!",e.updated_at_ms AS "updated_at_ms!",a.desired_json AS "desired_json!",a.generation AS "generation!",a.delete_intent AS "application_delete_intent!",a.created_at_ms AS "application_created_at_ms!",a.updated_at_ms AS "application_updated_at_ms!" FROM environments e JOIN applications a ON a.id=e.application_id WHERE e.id=?1"#,id)
            .fetch_optional(connection).await.map_err(StoreError::database)?
            .map(StoredEnvironmentRow::decode).transpose()
    }

    /// Reads an environment and its status from one database snapshot.
    ///
    /// # Errors
    /// Returns a storage error or `NotFound`.
    pub async fn get_with_status(
        &self,
        id: &EnvironmentId,
    ) -> Result<(StoredEnvironment, EnvironmentStatus), StoreError> {
        let mut tx = self.pool.begin().await.map_err(StoreError::database)?;
        let environment = Self::environment_on(&mut tx, id.as_str())
            .await?
            .ok_or(StoreError::NotFound)?;
        let status = Self::status_on(&mut tx, id).await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok((environment, status))
    }

    /// Lists live environments by ID. Corrupt rows are logged and skipped.
    ///
    /// # Errors
    /// Returns a storage error or an invalid pagination error.
    pub async fn list(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<EnvironmentPage, StoreError> {
        let fetch_limit = page_limit(limit)? + 1;
        let after = cursor
            .map(|value| value.strip_prefix("v1:").ok_or(StoreError::InvalidInput))
            .transpose()?
            .unwrap_or("");
        let mut rows = sqlx::query_as!(StoredEnvironmentRow,
            r#"SELECT e.id AS "id!",e.application_id AS "application_id!",e.name AS "name!",e.resolved_json,e.resolved_generation,e.delete_intent AS "delete_intent!",e.created_at_ms AS "created_at_ms!",e.updated_at_ms AS "updated_at_ms!",a.desired_json AS "desired_json!",a.generation AS "generation!",a.delete_intent AS "application_delete_intent!",a.created_at_ms AS "application_created_at_ms!",a.updated_at_ms AS "application_updated_at_ms!" FROM environments e JOIN applications a ON a.id=e.application_id WHERE e.id>?1 ORDER BY e.id LIMIT ?2"#,after,fetch_limit)
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
                Ok(environment) => Some(environment),
                Err(error) => { tracing::error!(environment_id=%id,%error,"quarantined undecodable environment row"); None }
            }
        }).collect();
        Ok(EnvironmentPage { items, next_cursor })
    }
}
