//! Applications: the saved manifest their environments share, and atomic
//! revision checks.
use super::{
    ApplicationId, ApplicationRow, ApplicationState, EnvironmentRow, Operation, OperationKind,
    Paging, ResolvedApplication, Store, StoreError, StoredApplication, now_ms,
};
use piqueld_core::manifest::ApplicationTemplate;
use piqueld_core::{
    EnvironmentId, EnvironmentName, EnvironmentSource,
    api::{ApplicationSummary, EnvironmentView, Page},
};
use sqlx::{Sqlite, SqliteConnection, Transaction};

/// The `kind` column of `environments`.
#[derive(Clone, Copy)]
enum Kind {
    /// An environment.
    Environment,
    /// A preview of a branch.
    Preview,
}

impl Kind {
    /// The stored value.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::Preview => "preview",
        }
    }
}

impl Store {
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
        let now = now_ms();
        let operation =
            Self::insert_operation(&mut tx, &environment, OperationKind::Apply, now).await?;
        let mut resolved = resolved.cloned();
        if let Some(resolved) = &mut resolved {
            Self::bind_links_on(&mut tx, resolved).await?;
        }
        let resolved = resolved
            .as_ref()
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
        Self::write_status(
            &mut tx,
            environment.as_str(),
            ApplicationState::Pending,
            now,
        )
        .await?;
        Self::commit_environment_changes(tx, [environment.as_str()]).await?;
        Ok(operation)
    }

    /// Reads a live application.
    ///
    /// # Errors
    /// Returns a storage error or `NotFound`.
    pub async fn application(&self, id: &ApplicationId) -> Result<StoredApplication, StoreError> {
        Self::application_on(&mut *self.connection().await?, id.as_str())
            .await?
            .ok_or(StoreError::NotFound)
    }

    /// The manifest a repository-backed application last read from the
    /// branch and commit `spec.manifest` names, with its current connection:
    /// where its saved manifest differs from it, it was edited since. `None`
    /// unless repository-backed and until a deployment read one.
    ///
    /// # Errors
    /// Returns a storage or decoding error.
    pub async fn repository_manifest(
        &self,
        application: &StoredApplication,
    ) -> Result<Option<ApplicationTemplate>, StoreError> {
        Self::repository_manifest_on(&mut *self.connection().await?, application).await
    }

    /// See [`Self::repository_manifest`].
    pub(super) async fn repository_manifest_on(
        connection: &mut SqliteConnection,
        application: &StoredApplication,
    ) -> Result<Option<ApplicationTemplate>, StoreError> {
        let id = application.application.id().as_str();
        let json = sqlx::query_scalar!("SELECT repository_json FROM applications WHERE id=?1", id)
            .fetch_optional(connection)
            .await
            .map_err(StoreError::database)?
            .flatten();
        let Some(connection) = application.application.spec().manifest.clone() else {
            return Ok(None);
        };
        json.map(|json| {
            let repository: ApplicationTemplate =
                serde_json::from_str(&json).map_err(StoreError::corrupt)?;
            Ok(repository
                .with_name(application.application.metadata().name.clone())
                .with_manifest(Some(connection)))
        })
        .transpose()
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
        Self::find_by_name_on(&mut *self.connection().await?, name).await
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

    /// Lists an application's environments, never its previews, in name order.
    ///
    /// # Errors
    /// Returns a storage or decoding error. Absent applications have none.
    pub async fn environments(
        &self,
        id: &ApplicationId,
    ) -> Result<Vec<EnvironmentView>, StoreError> {
        Self::environments_on(&mut *self.connection().await?, id.as_str()).await
    }

    /// Lists an application's environments on an existing connection or
    /// transaction, never its previews.
    pub(super) async fn environments_on(
        connection: &mut SqliteConnection,
        id: &str,
    ) -> Result<Vec<EnvironmentView>, StoreError> {
        Self::deployables_of_kind_on(connection, id, Kind::Environment).await
    }

    /// Lists an application's previews on an existing connection or transaction.
    pub(super) async fn previews_on(
        connection: &mut SqliteConnection,
        id: &str,
    ) -> Result<Vec<EnvironmentView>, StoreError> {
        Self::deployables_of_kind_on(connection, id, Kind::Preview).await
    }

    /// Lists an application's environments, then its previews: everything
    /// deployed under it.
    pub(super) async fn deployables_on(
        connection: &mut SqliteConnection,
        id: &str,
    ) -> Result<Vec<EnvironmentView>, StoreError> {
        let mut all = Self::environments_on(connection, id).await?;
        all.extend(Self::previews_on(connection, id).await?);
        Ok(all)
    }

    /// Lists an application's environments of `kind`, in name order.
    async fn deployables_of_kind_on(
        connection: &mut SqliteConnection,
        id: &str,
        kind: Kind,
    ) -> Result<Vec<EnvironmentView>, StoreError> {
        let kind = kind.as_str();
        sqlx::query_as!(EnvironmentRow,
            r#"SELECT e.id AS "id!",e.application_id AS "application_id!",e.name AS "name!",e.kind AS "kind!",e.branch,e.pinned_commit,e.promoted_from,e.preview_slot,e.sync AS "sync!",e.telemetry AS "telemetry!",e.stopped AS "stopped!",e.synced_commit,e.synced_at_ms,e.resolved_generation,e.delete_intent AS "delete_intent!",e.created_at_ms AS "created_at_ms!",e.updated_at_ms AS "updated_at_ms!" FROM environments e WHERE e.application_id=?1 AND e.kind=?2 ORDER BY e.name"#,id,kind)
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
        Self::insert_environment_on(tx, application.id(), (&id, &name), &source, false, now)
            .await?;
        Ok(id)
    }

    /// Keeps tracking environments' sources in step with the application's
    /// repository connection: connecting points every one without a branch at
    /// the branch `spec.manifest` names, and disconnecting returns every one
    /// to the saved manifest, forgetting what was fetched. Promoted
    /// environments keep promoting. Previews keep their branch: they cannot
    /// deploy until a repository is connected again. Leaving `previous_url`
    /// forgets the heads sync followed from, so nothing follows the new
    /// repository before its next deployment.
    pub(super) async fn follow_connection_on(
        tx: &mut Transaction<'_, Sqlite>,
        application: &ApplicationTemplate,
        previous_url: Option<&str>,
    ) -> Result<(), StoreError> {
        let id = application.id().as_str();
        let url = application
            .spec()
            .manifest
            .as_ref()
            .map(|connection| connection.repository.url.as_str());
        if url != previous_url {
            sqlx::query!(
                "UPDATE environments SET synced_commit=NULL,synced_at_ms=NULL WHERE application_id=?1",
                id
            )
            .execute(&mut **tx)
            .await
            .map_err(StoreError::database)?;
        }
        let source = EnvironmentSource::select(application.spec().manifest.as_ref(), None)?;
        match source.branch() {
            Some(branch) => {
                let (name, commit) = (branch.branch(), branch.commit());
                sqlx::query!(
                    "UPDATE environments SET branch=?1,pinned_commit=?2 WHERE application_id=?3 AND branch IS NULL AND promoted_from IS NULL AND kind='environment'",
                    name,
                    commit,
                    id
                )
                .execute(&mut **tx)
                .await
            }
            None => {
                sqlx::query!(
                    "UPDATE environments SET branch=NULL,pinned_commit=NULL,manifest_json=NULL WHERE application_id=?1 AND promoted_from IS NULL AND kind='environment'",
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
        let paging = Paging::new(limit)?;
        let after = Paging::cursor(cursor)?
            .map(ApplicationId::parse)
            .transpose()
            .map_err(StoreError::invalid_input)?;
        let after = after.as_ref().map_or("", ApplicationId::as_str);
        let visible = super::auth::access::scope_json(visible);
        let mut tx = self.snapshot().await?;
        let mut rows = sqlx::query!(
            r#"SELECT id AS "id!",name,generation,delete_intent,created_at_ms,updated_at_ms FROM applications WHERE id>?1 AND (?3 IS NULL OR id IN (SELECT value FROM json_each(?3))) ORDER BY id LIMIT ?2"#,
            after,
            paging.fetch,
            visible
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let next_cursor = paging.next_cursor(&mut rows, |row| row.id.clone());
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
