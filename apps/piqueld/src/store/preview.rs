//! Previews: environments of the preview kind, deploying one branch each.
//! Their lifecycle never needs or advances the application revision, since
//! they select no `[spec.environments.<name>]` block; creation is idempotent
//! through the `preview_key` unique index instead.
use super::{
    EnvironmentRow, Operation, ResolvedApplication, Store, StoreError, StoredApplication,
    StoredEnvironment, now_ms,
};
use piqueld_core::{
    ApplicationId, EnvironmentId, GitBranch, PreviewSlot, PreviewSlug, api::EnvironmentView,
};
use sqlx::{Sqlite, SqliteConnection, Transaction};

impl Store {
    /// Lists an application's previews in slug order.
    ///
    /// # Errors
    /// Returns a storage or decoding error. Absent applications have none.
    pub async fn previews(&self, id: &ApplicationId) -> Result<Vec<EnvironmentView>, StoreError> {
        Self::previews_on(
            &mut *self.pool.acquire().await.map_err(StoreError::database)?,
            id.as_str(),
        )
        .await
    }

    /// Lists an application's previews on an existing connection or transaction.
    pub(super) async fn previews_on(
        connection: &mut SqliteConnection,
        id: &str,
    ) -> Result<Vec<EnvironmentView>, StoreError> {
        sqlx::query_as!(EnvironmentRow,
            r#"SELECT e.id AS "id!",e.application_id AS "application_id!",e.name AS "name!",e.kind AS "kind!",e.branch,e.pinned_commit,e.preview_slot,e.resolved_generation,e.delete_intent AS "delete_intent!",e.created_at_ms AS "created_at_ms!",e.updated_at_ms AS "updated_at_ms!" FROM environments e WHERE e.application_id=?1 AND e.kind='preview' ORDER BY e.name"#,id)
            .fetch_all(connection).await.map_err(StoreError::database)?
            .into_iter().map(EnvironmentRow::decode).collect()
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

    /// Creates the preview of `branch` and `slot`, named by the slug derived
    /// from them, unless one exists. Returns its ID and whether it was created.
    /// The application must be repository-backed (`PreviewRequiresRepository`)
    /// and not being deleted (`Busy`); so must an existing preview. A slug
    /// already naming an environment is `AlreadyExists`.
    pub(super) async fn create_preview_on(
        tx: &mut Transaction<'_, Sqlite>,
        application: &StoredApplication,
        branch: &GitBranch,
        slot: Option<&PreviewSlot>,
        now: i64,
    ) -> Result<(EnvironmentId, bool), StoreError> {
        if application.delete_intent {
            return Err(StoreError::Busy);
        }
        let template = &application.application;
        if template.spec().manifest.is_none() {
            return Err(StoreError::PreviewRequiresRepository);
        }
        let slug = PreviewSlug::derive(&template.metadata().name, branch, slot);
        let id = super::new_id("preview");
        let (application_id, slug, branch, slot) = (
            template.id().as_str(),
            slug.as_str(),
            branch.as_str(),
            slot.map(PreviewSlot::as_str),
        );
        // Any unique conflict leaves the row out: the same branch and slot
        // (found below), or a slug naming another environment (not found).
        let created = sqlx::query!(
            "INSERT INTO environments(id,application_id,name,kind,branch,preview_slot,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,'preview',?4,?5,?6,?6) ON CONFLICT DO NOTHING",
            id,
            application_id,
            slug,
            branch,
            slot,
            now
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected()
            == 1;
        let (id, delete_intent) = sqlx::query!(
            r#"SELECT id AS "id!",delete_intent AS "delete_intent!" FROM environments WHERE application_id=?1 AND kind='preview' AND branch=?2 AND COALESCE(preview_slot,'')=COALESCE(?3,'')"#,
            application_id,
            branch,
            slot
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)?
        .map(|row| (row.id, row.delete_intent != 0))
        .ok_or(StoreError::AlreadyExists)?;
        if delete_intent {
            return Err(StoreError::Busy);
        }
        if created {
            Self::write_status(tx, &id, "not_deployed", None, now).await?;
            let message = format!("created preview {slug} of branch {branch}");
            sqlx::query!(
                "INSERT INTO events(application_id,environment_id,kind,message,created_at_ms) VALUES(?1,?2,'preview_created',?3,?4)",
                application_id,
                id,
                message,
                now
            )
            .execute(&mut **tx)
            .await
            .map_err(StoreError::database)?;
        }
        Ok((
            EnvironmentId::parse(id).map_err(StoreError::corrupt)?,
            created,
        ))
    }

    /// Loads a preview for a change. Environments are `NotFound`, as previews
    /// are for environment changes.
    pub(super) async fn preview_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: &EnvironmentId,
    ) -> Result<StoredEnvironment, StoreError> {
        Self::environment_on(tx, id.as_str())
            .await?
            .filter(|preview| preview.environment.preview().is_some())
            .ok_or(StoreError::NotFound)
    }

    /// Starts a deployment of the head of a preview's branch. Fails with
    /// `PreviewRequiresRepository` once its application was disconnected.
    pub(super) async fn deploy_preview_on(
        tx: &mut Transaction<'_, Sqlite>,
        preview: &StoredEnvironment,
    ) -> Result<Operation, StoreError> {
        if preview.repository().is_none() {
            return Err(StoreError::PreviewRequiresRepository);
        }
        let operation = Self::request_deploy_on(tx, preview).await?;
        Self::insert_deployment_on(tx, &operation, &preview.candidate(None)?).await?;
        Ok(operation)
    }

    /// Adds the Docker volumes of a preview's prepared target to its
    /// inventory, before any of them is created.
    pub(super) async fn record_preview_volumes_on(
        tx: &mut Transaction<'_, Sqlite>,
        resolved: &ResolvedApplication,
    ) -> Result<(), StoreError> {
        let (id, now) = (resolved.id.as_str(), now_ms());
        for volume in &resolved.volumes {
            let name = volume.name.as_str();
            sqlx::query!(
                "INSERT INTO preview_volumes(environment_id,name,created_at_ms) VALUES(?1,?2,?3) ON CONFLICT DO NOTHING",
                id,
                name,
                now
            )
            .execute(&mut **tx)
            .await
            .map_err(StoreError::database)?;
        }
        Ok(())
    }

    /// Every Docker volume a preview's deployments created, including
    /// volumes later manifests dropped.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn preview_volumes(&self, id: &EnvironmentId) -> Result<Vec<String>, StoreError> {
        let id = id.as_str();
        sqlx::query_scalar!(
            "SELECT name FROM preview_volumes WHERE environment_id=?1 ORDER BY name",
            id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)
    }

    /// The commit a preview's latest fetched deployment read its manifest from.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn fetched_commit(&self, id: &EnvironmentId) -> Result<Option<String>, StoreError> {
        let id = id.as_str();
        Ok(sqlx::query_scalar!(
            "SELECT i.repository_commit FROM deployment_inputs i JOIN operations o ON o.id=i.operation_id WHERE o.environment_id=?1 AND i.fetched=1 AND i.repository_commit IS NOT NULL ORDER BY o.created_at_ms DESC,o.id DESC LIMIT 1",
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        .flatten())
    }
}
