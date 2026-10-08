//! Applications: the saved manifest their environments share, and atomic
//! revision checks.
use super::{
    ApplicationId, ApplicationRow, EnvironmentRow, Operation, OperationKind, ResolvedApplication,
    Store, StoreError, StoredApplication, now_ms, page_limit,
};
use piqueld_core::manifest::ApplicationTemplate;
use piqueld_core::{
    EnvironmentId, EnvironmentName, EnvironmentSource,
    api::{ApplicationSummary, EnvironmentView, Page},
};
use sqlx::{Sqlite, SqliteConnection, Transaction};

impl Store {
    /// Parses an opaque application page cursor into the last ID already returned.
    ///
    /// ```text
    /// "v1:app-0192f1c0..." -> Some(ApplicationId("app-0192f1c0..."))
    /// ```
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

    /// Reads an application's current generation (zero when absent) and
    /// checks it against the caller's optional precondition.
    pub(super) async fn generation_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: &str,
        expected: Option<u64>,
    ) -> Result<i64, StoreError> {
        let actual = sqlx::query_scalar!("SELECT generation FROM applications WHERE id=?1", id)
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

    /// Stores changed intent and requests an apply operation for the
    /// application's only environment, creating it when the application is new.
    /// Optional resolved state supports importing an already prepared target.
    /// # Errors
    /// Returns storage, revision, name collision, environment selection, or
    /// pending-deletion errors.
    pub async fn save_application(
        &self,
        app: &ApplicationTemplate,
        resolved: Option<&ResolvedApplication>,
        expected: Option<u64>,
    ) -> Result<Operation, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let saved = Self::save_configuration_on(&mut tx, app, expected).await?;
        let environment = Self::sole_environment_on(&mut tx, app.id()).await?;
        let operation =
            Self::insert_operation(&mut tx, &environment, OperationKind::Apply, now_ms()).await?;
        let resolved = resolved
            .map(serde_json::to_string)
            .transpose()
            .map_err(StoreError::corrupt)?;
        if let Some(resolved) = &resolved {
            let id = environment.as_str();
            let generation = i64::try_from(saved.generation).map_err(StoreError::corrupt)?;
            sqlx::query!(
                "UPDATE environments SET resolved_json=?1,resolved_generation=?2 WHERE id=?3",
                resolved,
                generation,
                id
            )
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        }
        sqlx::query!(
            "UPDATE operations SET target_json=?1 WHERE id=?2",
            resolved,
            operation.id
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        Self::write_status(&mut tx, environment.as_str(), "pending", None, now_ms()).await?;
        Self::commit_environment_changes(tx, [environment.as_str()]).await?;
        Ok(operation)
    }

    /// Reads a live application.
    ///
    /// # Errors
    /// Returns a storage error or `NotFound`.
    pub async fn application(&self, id: &ApplicationId) -> Result<StoredApplication, StoreError> {
        Self::application_on(
            &mut *self.pool.acquire().await.map_err(StoreError::database)?,
            id.as_str(),
        )
        .await?
        .ok_or(StoreError::NotFound)
    }

    /// Reads an application on an existing connection or transaction.
    pub(super) async fn application_on(
        connection: &mut SqliteConnection,
        id: &str,
    ) -> Result<Option<StoredApplication>, StoreError> {
        sqlx::query_as!(ApplicationRow,
            r#"SELECT id AS "id!",desired_json,generation,delete_intent,created_at_ms,updated_at_ms FROM applications WHERE id=?1"#,id)
            .fetch_optional(connection).await.map_err(StoreError::database)?
            .map(ApplicationRow::decode).transpose()
    }

    /// Finds a live application by manifest name, including applications being deleted.
    ///
    /// # Errors
    /// Returns a storage or decoding error.
    pub async fn find_by_name(&self, name: &str) -> Result<Option<StoredApplication>, StoreError> {
        Self::find_by_name_on(
            &mut *self.pool.acquire().await.map_err(StoreError::database)?,
            name,
        )
        .await
    }

    /// Finds an application by name on an existing connection or transaction.
    pub(super) async fn find_by_name_on(
        connection: &mut SqliteConnection,
        name: &str,
    ) -> Result<Option<StoredApplication>, StoreError> {
        sqlx::query_as!(ApplicationRow,
            r#"SELECT id AS "id!",desired_json,generation,delete_intent,created_at_ms,updated_at_ms FROM applications WHERE name=?1"#,name)
            .fetch_optional(connection).await.map_err(StoreError::database)?
            .map(ApplicationRow::decode).transpose()
    }

    /// The ID of the application named `name`, if any.
    pub(super) async fn application_id_by_name_on(
        connection: &mut SqliteConnection,
        name: &str,
    ) -> Result<Option<ApplicationId>, StoreError> {
        sqlx::query_scalar!(
            r#"SELECT id AS "id!" FROM applications WHERE name=?1"#,
            name
        )
        .fetch_optional(connection)
        .await
        .map_err(StoreError::database)?
        .map(|id| ApplicationId::parse(id).map_err(StoreError::corrupt))
        .transpose()
    }

    /// Lists an application's environments in name order.
    ///
    /// # Errors
    /// Returns a storage or decoding error. Absent applications have none.
    pub async fn environments(
        &self,
        id: &ApplicationId,
    ) -> Result<Vec<EnvironmentView>, StoreError> {
        Self::environments_on(
            &mut *self.pool.acquire().await.map_err(StoreError::database)?,
            id.as_str(),
        )
        .await
    }

    /// Lists an application's environments on an existing connection or transaction.
    pub(super) async fn environments_on(
        connection: &mut SqliteConnection,
        id: &str,
    ) -> Result<Vec<EnvironmentView>, StoreError> {
        sqlx::query_as!(EnvironmentRow,
            r#"SELECT e.id AS "id!",e.application_id AS "application_id!",e.name AS "name!",e.branch,e.pinned_commit,e.resolved_generation,e.delete_intent AS "delete_intent!",e.created_at_ms AS "created_at_ms!",e.updated_at_ms AS "updated_at_ms!" FROM environments e WHERE e.application_id=?1 ORDER BY e.name"#,id)
            .fetch_all(connection).await.map_err(StoreError::database)?
            .into_iter().map(EnvironmentRow::decode).collect()
    }

    /// Selects the environment runtime commands use when none is named: the
    /// application's only environment. Never picks one of several silently.
    pub(super) async fn sole_environment_on(
        tx: &mut Transaction<'_, Sqlite>,
        application: &ApplicationId,
    ) -> Result<EnvironmentId, StoreError> {
        let mut environments = Self::environments_on(tx, application.as_str()).await?;
        if environments.len() == 1
            && let Some(environment) = environments.pop()
        {
            return Ok(environment.id);
        }
        Err(StoreError::EnvironmentRequired {
            environments: environments
                .into_iter()
                .map(|environment| environment.name)
                .collect(),
        })
    }

    /// Creates the application's first environment, named `production`, which
    /// shares the application's ID and follows the branch `spec.manifest` names.
    pub(super) async fn create_default_environment_on(
        tx: &mut Transaction<'_, Sqlite>,
        application: &ApplicationTemplate,
        now: i64,
    ) -> Result<EnvironmentId, StoreError> {
        let id = EnvironmentId::default_for(application.id());
        let source = EnvironmentSource::select(application.spec().manifest.as_ref(), None)?;
        let name = EnvironmentName::default_name();
        Self::insert_environment_on(tx, application.id(), &id, &name, &source, now).await?;
        Ok(id)
    }

    /// Keeps environment sources in step with the application's repository
    /// connection: connecting points every environment without a branch at the
    /// branch `spec.manifest` names, and disconnecting returns every environment
    /// to the saved manifest, forgetting what was fetched.
    pub(super) async fn follow_connection_on(
        tx: &mut Transaction<'_, Sqlite>,
        application: &ApplicationTemplate,
    ) -> Result<(), StoreError> {
        let id = application.id().as_str();
        let source = EnvironmentSource::select(application.spec().manifest.as_ref(), None)?;
        match source.branch() {
            Some(branch) => {
                let (name, commit) = (branch.branch(), branch.commit());
                sqlx::query!(
                    "UPDATE environments SET branch=?1,pinned_commit=?2 WHERE application_id=?3 AND branch IS NULL",
                    name,
                    commit,
                    id
                )
                .execute(&mut **tx)
                .await
            }
            None => {
                sqlx::query!(
                    "UPDATE environments SET branch=NULL,pinned_commit=NULL,manifest_json=NULL WHERE application_id=?1",
                    id
                )
                .execute(&mut **tx)
                .await
            }
        }
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// Lists live application metadata and environments by ID, within
    /// `visible`, without reading manifest documents.
    ///
    /// # Errors
    /// Returns a storage error or an invalid pagination error.
    pub async fn list_summaries(
        &self,
        visible: &piqueld_core::access::Scope,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<ApplicationSummary>, StoreError> {
        let fetch_limit = page_limit(limit)? + 1;
        let after = Self::application_cursor(cursor)?;
        let after = after.as_ref().map_or("", ApplicationId::as_str);
        let visible = super::access::scope_json(visible);
        let mut tx = self.pool.begin().await.map_err(StoreError::database)?;
        let mut rows = sqlx::query!(
            r#"SELECT id AS "id!",name,generation,delete_intent,created_at_ms,updated_at_ms FROM applications WHERE id>?1 AND (?3 IS NULL OR id IN (SELECT value FROM json_each(?3))) ORDER BY id LIMIT ?2"#,
            after,
            fetch_limit,
            visible
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = if has_more {
            rows.last().map(|row| format!("v1:{}", row.id))
        } else {
            None
        };
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            items.push(ApplicationSummary {
                environments: Self::environments_on(&mut tx, &row.id).await?,
                id: ApplicationId::parse(row.id).map_err(StoreError::corrupt)?,
                name: row.name,
                generation: u64::try_from(row.generation).map_err(StoreError::corrupt)?,
                delete_intent: row.delete_intent != 0,
                created_at_ms: row.created_at_ms,
                updated_at_ms: row.updated_at_ms,
            });
        }
        tx.commit().await.map_err(StoreError::database)?;
        Ok(Page { items, next_cursor })
    }
}
