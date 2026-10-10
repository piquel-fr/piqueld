//! Environments: deployable units with their own operations, status, and
//! resolved targets, deploying their application's shared manifest or
//! releases promoted from another environment.
use super::{
    ApplicationId, EnvironmentId, EnvironmentPage, EnvironmentStatus, Operation, OperationKind,
    ResolvedApplication, Store, StoreError, StoredEnvironment, StoredEnvironmentRow, now_ms,
    page_limit,
};
use crate::api::SourceChoice;
use piqueld_core::{
    EnvironmentKind, EnvironmentName, EnvironmentSource, PromotedFrom, TrackedBranch,
    manifest::RenderTarget,
};
use sqlx::{Sqlite, SqliteConnection, Transaction};

impl Store {
    /// Inserts an environment deploying from `source`, initially
    /// `not_deployed`, and records its creation. Returns `AlreadyExists` when
    /// the application already has one with `name`.
    pub(super) async fn insert_environment_on(
        tx: &mut Transaction<'_, Sqlite>,
        application: &ApplicationId,
        id: &EnvironmentId,
        name: &EnvironmentName,
        source: &EnvironmentSource,
        now: i64,
    ) -> Result<(), StoreError> {
        let (application, id, name) = (application.as_str(), id.as_str(), name.as_str());
        let (branch, commit, promoted_from) = Self::source_columns(source);
        sqlx::query!(
            "INSERT INTO environments(id,application_id,name,branch,pinned_commit,promoted_from,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?7)",
            id,
            application,
            name,
            branch,
            commit,
            promoted_from,
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

    /// The `branch`, `pinned_commit`, and `promoted_from` columns of `source`.
    fn source_columns(source: &EnvironmentSource) -> (Option<&str>, Option<&str>, Option<&str>) {
        let branch = source.branch();
        (
            branch.map(TrackedBranch::branch),
            branch.and_then(TrackedBranch::commit),
            source.promoted_from().map(EnvironmentId::as_str),
        )
    }

    /// Creates an environment of a live application under a freshly generated
    /// ID, advancing the application revision. It promotes from another
    /// environment (see `check_promotion_source_on`), or follows a branch, by
    /// default the one `spec.manifest` names, when the application is
    /// repository-backed. Applications being deleted are `Busy`.
    pub(super) async fn create_environment_on(
        tx: &mut Transaction<'_, Sqlite>,
        application: &super::StoredApplication,
        name: &EnvironmentName,
        source: Option<SourceChoice>,
        now: i64,
    ) -> Result<EnvironmentId, StoreError> {
        if application.delete_intent {
            return Err(StoreError::Busy);
        }
        let template = &application.application;
        let connection = template.spec().manifest.as_ref();
        let source = match source {
            Some(SourceChoice::PromoteFrom(environment)) => {
                EnvironmentSource::Promoted(PromotedFrom { environment })
            }
            Some(SourceChoice::Branch(branch)) => {
                EnvironmentSource::select(connection, Some(branch))?
            }
            None => EnvironmentSource::select(connection, None)?,
        };
        let id = EnvironmentId::parse(super::new_id("env")).map_err(StoreError::corrupt)?;
        if let Some(from) = source.promoted_from() {
            Self::check_promotion_source_on(tx, template.id(), (&id, name), from).await?;
        }
        Self::insert_environment_on(tx, template.id(), &id, name, &source, now).await?;
        Self::bump_generation_on(
            tx,
            application.application.id(),
            application.generation,
            None,
        )
        .await?;
        Ok(id)
    }

    /// Renames an environment that is not being deleted, advancing the
    /// application revision. Its runtime is unaffected. Names select
    /// `[spec.environments.<name>]`, so a rename is refused while the saved
    /// manifest configures the old or the new name, rather than silently
    /// switching the configuration the environment deploys. It stays resolved
    /// unless its configuration renders the name, e.g. `${{ env.name }}`.
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
        // A branch environment's configuration is its last fetched manifest;
        // before the first fetch there is none to switch.
        let manifest = environment.manifest();
        if let Some(configured) = [old, name]
            .into_iter()
            .find(|name| manifest.is_some_and(|manifest| manifest.configures(name)))
        {
            return Err(StoreError::EnvironmentConfigured {
                environment: configured.clone(),
            });
        }
        let stale = manifest
            .is_some_and(|manifest| {
                !manifest.renders_like(
                    &RenderTarget::Environment(old.clone()),
                    manifest,
                    &RenderTarget::Environment(name.clone()),
                )
            })
            .then(|| environment.id());
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
            &environment.environment.application_id,
            environment.application.generation,
            stale,
        )
        .await
    }

    /// Points an environment of a repository-backed application at `branch`
    /// (see `set_source_on`).
    pub(super) async fn set_branch_on(
        tx: &mut Transaction<'_, Sqlite>,
        environment: &StoredEnvironment,
        branch: TrackedBranch,
        now: i64,
    ) -> Result<(), StoreError> {
        let connection = environment.application.application.spec().manifest.as_ref();
        let source = EnvironmentSource::select(connection, Some(branch))?;
        Self::set_source_on(tx, environment, source, now).await
    }

    /// Makes an environment promoted from `promote_from` (see
    /// `check_promotion_source_on`), or, without one, returns it to tracking
    /// the application's saved manifest or the branch `spec.manifest` names
    /// (see `set_source_on`).
    pub(super) async fn set_promotion_on(
        tx: &mut Transaction<'_, Sqlite>,
        environment: &StoredEnvironment,
        promote_from: Option<EnvironmentId>,
        now: i64,
    ) -> Result<(), StoreError> {
        let source = match promote_from {
            Some(from) => {
                Self::check_promotion_source_on(
                    tx,
                    &environment.environment.application_id,
                    (environment.id(), &environment.environment.name),
                    &from,
                )
                .await?;
                EnvironmentSource::Promoted(PromotedFrom { environment: from })
            }
            None if environment.environment.source.promoted_from().is_some() => {
                let connection = environment.application.application.spec().manifest.as_ref();
                EnvironmentSource::select(connection, None)?
            }
            // Already tracking: keep its branch.
            None => environment.environment.source.clone(),
        };
        Self::set_source_on(tx, environment, source, now).await
    }

    /// Changes where an environment deploys from, advancing the application
    /// revision. Nothing is fetched or deployed: the environment no longer
    /// reports its configuration as resolved until its next deployment or
    /// promotion. Environments being deleted are `Busy`, and so are
    /// environments with a deployment in progress becoming promoted: that
    /// deployment may already be fetching or building.
    async fn set_source_on(
        tx: &mut Transaction<'_, Sqlite>,
        environment: &StoredEnvironment,
        source: EnvironmentSource,
        now: i64,
    ) -> Result<(), StoreError> {
        if environment.delete_intent() {
            return Err(StoreError::Busy);
        }
        let previous = &environment.environment.source;
        if source == *previous {
            return Ok(());
        }
        let id = environment.id().as_str();
        let deploying = sqlx::query_scalar!(
            "SELECT EXISTS(SELECT 1 FROM operations WHERE environment_id=?1 AND state IN ('requested','running'))",
            id
        )
        .fetch_one(&mut **tx)
        .await
        .map_err(StoreError::database)?
            != 0;
        if deploying && source.promoted_from().is_some() {
            return Err(StoreError::Busy);
        }
        let (branch, commit, promoted_from) = Self::source_columns(&source);
        sqlx::query!(
            // What sync found on the former branch says nothing about this one.
            "UPDATE environments SET branch=?1,pinned_commit=?2,promoted_from=?3,synced_commit=NULL,synced_at_ms=NULL,updated_at_ms=?4 WHERE id=?5",
            branch,
            commit,
            promoted_from,
            now,
            id
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        let message = format!(
            "environment {} deploys from {} instead of {}",
            environment.environment.name,
            Self::describe_source_on(tx, &source).await?,
            Self::describe_source_on(tx, previous).await?,
        );
        sqlx::query!(
            "INSERT INTO events(application_id,environment_id,kind,message,created_at_ms) VALUES((SELECT application_id FROM environments WHERE id=?1),?1,'environment_source_changed',?2,?3)",
            id,
            message,
            now
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Self::bump_generation_on(
            tx,
            &environment.environment.application_id,
            environment.application.generation,
            Some(environment.id()),
        )
        .await
    }

    /// Describes `source` in a sentence, naming a promotion source.
    async fn describe_source_on(
        connection: &mut SqliteConnection,
        source: &EnvironmentSource,
    ) -> Result<String, StoreError> {
        let Some(from) = source.promoted_from() else {
            return Ok(source.to_string());
        };
        let id = from.as_str();
        let name = sqlx::query_scalar!("SELECT name FROM environments WHERE id=?1", id)
            .fetch_optional(connection)
            .await
            .map_err(StoreError::database)?;
        Ok(format!(
            "releases promoted from {}",
            name.as_deref().unwrap_or(id)
        ))
    }

    /// Persists an environment's deletion intent and requests its delete
    /// operation. Callers advance the application revision. Returns
    /// `IllegalTransition` when deletion is already pending.
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

    /// Requests deletion of one environment and advances its application
    /// revision, since environment names select configuration. Refused while
    /// live environments promote from it.
    pub(super) async fn delete_environment_on(
        tx: &mut Transaction<'_, Sqlite>,
        environment: &StoredEnvironment,
    ) -> Result<Operation, StoreError> {
        Self::check_promotion_dependents_on(tx, environment.id()).await?;
        let operation = Self::request_delete_on(tx, environment.id()).await?;
        Self::bump_generation_on(
            tx,
            &environment.environment.application_id,
            environment.application.generation,
            None,
        )
        .await?;
        Ok(operation)
    }

    /// Deletes an environment without a precondition. See `delete_environment_on`.
    /// # Errors
    /// Returns storage, absence, or illegal transition errors.
    pub async fn request_delete(&self, id: &EnvironmentId) -> Result<Operation, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let environment = Self::environment_on(&mut tx, id.as_str())
            .await?
            .ok_or(StoreError::NotFound)?;
        let result = Self::delete_environment_on(&mut tx, &environment).await?;
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
    /// Stores the target on the running, latest operation, records any fetched
    /// repository manifest as the environment's own and the release the target
    /// runs, adds a preview's volumes to its inventory before they exist, and
    /// records `target_resolved`.
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
        // Boxed: the read would otherwise grow every preparation's future.
        let kind = Box::pin(Self::environment_on(
            &mut tx,
            operation.environment_id.as_str(),
        ))
        .await?
        .ok_or(StoreError::NotFound)?
        .environment
        .kind;
        Self::accept_deployment_on(&mut tx, operation, &kind).await?;
        if let EnvironmentKind::Preview(_) = kind {
            Self::record_preview_volumes_on(&mut tx, resolved).await?;
        }
        let now = now_ms();
        Self::record_release_on(
            &mut tx,
            &operation.id,
            &operation.environment_id,
            resolved,
            now,
        )
        .await?;
        Self::operation_event(&mut tx, &operation.id, "target_resolved", None, now).await?;
        Self::commit_environment_changes(tx, [operation.environment_id.as_str()]).await
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

    /// The application owning environment `id`, if it exists.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn environment_application(
        &self,
        id: &EnvironmentId,
    ) -> Result<Option<ApplicationId>, StoreError> {
        Self::environment_application_on(
            &mut *self.pool.acquire().await.map_err(StoreError::database)?,
            id,
        )
        .await
    }

    /// [`Self::environment_application`] on an existing connection or transaction.
    pub(crate) async fn environment_application_on(
        connection: &mut SqliteConnection,
        id: &EnvironmentId,
    ) -> Result<Option<ApplicationId>, StoreError> {
        let id = id.as_str();
        sqlx::query_scalar!(
            r#"SELECT application_id AS "application_id!" FROM environments WHERE id=?1"#,
            id
        )
        .fetch_optional(connection)
        .await
        .map_err(StoreError::database)?
        .map(|id| ApplicationId::parse(id).map_err(StoreError::corrupt))
        .transpose()
    }

    /// Reads an environment on an existing connection or transaction.
    pub(super) async fn environment_on(
        connection: &mut SqliteConnection,
        id: &str,
    ) -> Result<Option<StoredEnvironment>, StoreError> {
        sqlx::query_as!(StoredEnvironmentRow,
            r#"SELECT e.id AS "id!",e.application_id AS "application_id!",e.name AS "name!",e.kind AS "kind!",e.branch,e.pinned_commit,e.promoted_from,e.preview_slot,e.sync AS "sync!",e.synced_commit,e.synced_at_ms,e.manifest_json,e.resolved_json,e.resolved_generation,e.delete_intent AS "delete_intent!",e.created_at_ms AS "created_at_ms!",e.updated_at_ms AS "updated_at_ms!",a.desired_json AS "desired_json!",a.generation AS "generation!",a.delete_intent AS "application_delete_intent!",a.created_at_ms AS "application_created_at_ms!",a.updated_at_ms AS "application_updated_at_ms!" FROM environments e JOIN applications a ON a.id=e.application_id WHERE e.id=?1"#,id)
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
            r#"SELECT e.id AS "id!",e.application_id AS "application_id!",e.name AS "name!",e.kind AS "kind!",e.branch,e.pinned_commit,e.promoted_from,e.preview_slot,e.sync AS "sync!",e.synced_commit,e.synced_at_ms,e.manifest_json,e.resolved_json,e.resolved_generation,e.delete_intent AS "delete_intent!",e.created_at_ms AS "created_at_ms!",e.updated_at_ms AS "updated_at_ms!",a.desired_json AS "desired_json!",a.generation AS "generation!",a.delete_intent AS "application_delete_intent!",a.created_at_ms AS "application_created_at_ms!",a.updated_at_ms AS "application_updated_at_ms!" FROM environments e JOIN applications a ON a.id=e.application_id WHERE e.id>?1 ORDER BY e.id LIMIT ?2"#,after,fetch_limit)
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
