//! Previews: environments of the preview kind, deploying one branch each.
//! Their lifecycle never needs or advances the application revision, since
//! they select no `[spec.environments.<name>]` block; creation is idempotent
//! through the `preview_key` unique index instead, and bounded by the
//! `[previews]` counts checked in the creating transaction.
use super::{
    EnvironmentRow, Operation, ResolvedApplication, Store, StoreError, StoredApplication,
    StoredEnvironment, now_ms,
};
use piqueld_core::{
    ApplicationId, EnvironmentId, GitBranch, Preview, PreviewSlot, PreviewSlug,
    access::Scope,
    api::{
        ApplicationPreviews, CountedPreview, DiagnosticView, EnvironmentView, LastDeployment,
        PreviewLimit, PreviewLimitReached, PreviewUsage,
    },
    codes,
    manifest::PreviewLimits,
};
use sqlx::{Sqlite, SqliteConnection, Transaction};

impl Store {
    /// Applies the daemon's `[previews]` limits.
    #[must_use]
    pub fn with_previews(mut self, limits: PreviewLimits) -> Self {
        self.previews = limits;
        self
    }

    /// The `[previews]` limits that preview creation and rendering apply.
    #[must_use]
    pub const fn preview_limits(&self) -> &PreviewLimits {
        &self.previews
    }

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
    /// already naming an environment is `AlreadyExists`. A new preview must
    /// fit `limits` (`PreviewLimitReached`); an existing one is returned
    /// whatever the counts.
    pub(super) async fn create_preview_on(
        tx: &mut Transaction<'_, Sqlite>,
        application: &StoredApplication,
        branch: &GitBranch,
        slot: Option<&PreviewSlot>,
        limits: &PreviewLimits,
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
            Self::check_preview_limits_on(tx, application_id, &id, limits).await?;
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

    /// Refuses `created`, the preview just inserted into `application`, when
    /// it takes the application's previews past `max_per_application`, or
    /// the installation's past `max_total`, listing the others it counts.
    /// Previews being deleted are not counted. Writers run one transaction
    /// at a time, so concurrent creations count each other's previews.
    /// Counts already over a lowered limit only refuse new previews.
    async fn check_preview_limits_on(
        tx: &mut Transaction<'_, Sqlite>,
        application: &str,
        created: &str,
        limits: &PreviewLimits,
    ) -> Result<(), StoreError> {
        let counts = sqlx::query!(
            r#"SELECT COUNT(*) AS "total!: i64",COALESCE(SUM(application_id=?1),0) AS "own!: i64" FROM environments WHERE kind='preview' AND delete_intent=0"#,
            application
        )
        .fetch_one(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        let (limit, max, scope) = if counts.own > i64::from(limits.max_per_application) {
            (
                PreviewLimit::PerApplication,
                limits.max_per_application,
                Some(application),
            )
        } else if counts.total > i64::from(limits.max_total) {
            (PreviewLimit::Total, limits.max_total, None)
        } else {
            return Ok(());
        };
        let previews = sqlx::query!(
            r#"SELECT e.id AS "id!",e.application_id AS "application_id!",e.name AS "slug!",e.branch AS "branch!",e.preview_slot,d.id AS "deployment_id?",d.created_at_ms AS "deployed_at_ms?" FROM environments e LEFT JOIN deployments d ON d.id=(SELECT MAX(id) FROM deployments WHERE environment_id=e.id) WHERE e.kind='preview' AND e.delete_intent=0 AND e.id!=?2 AND (?1 IS NULL OR e.application_id=?1) ORDER BY COALESCE(d.created_at_ms,e.created_at_ms),e.id"#,
            scope,
            created
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(StoreError::database)?
        .into_iter()
        .map(|row| {
            Ok(CountedPreview {
                id: EnvironmentId::parse(row.id).map_err(StoreError::corrupt)?,
                application_id: ApplicationId::parse(row.application_id)
                    .map_err(StoreError::corrupt)?,
                preview: Preview {
                    branch: GitBranch::parse(row.branch).map_err(StoreError::corrupt)?,
                    slot: row
                        .preview_slot
                        .map(PreviewSlot::parse)
                        .transpose()
                        .map_err(StoreError::corrupt)?,
                    slug: PreviewSlug::parse(row.slug).map_err(StoreError::corrupt)?,
                },
                last_deployment: row.deployment_id.zip(row.deployed_at_ms).map(
                    |(id, created_at_ms)| LastDeployment { id, created_at_ms },
                ),
            })
        })
        .collect::<Result<_, StoreError>>()?;
        Err(StoreError::PreviewLimitReached(Box::new(
            PreviewLimitReached {
                limit,
                max,
                previews,
            },
        )))
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

    /// How `[previews]` bounded a preview's current target: the default
    /// limit and replica cap warnings of the deployment it promoted last.
    ///
    /// # Errors
    /// Returns a storage or decoding error.
    pub async fn preview_bounds(
        &self,
        id: &EnvironmentId,
    ) -> Result<Vec<DiagnosticView>, StoreError> {
        let id = id.as_str();
        let warnings = sqlx::query_scalar!(
            "SELECT d.warnings_json FROM deployments d JOIN operations o ON o.id=d.id WHERE d.environment_id=?1 AND o.promoted=1 ORDER BY d.id DESC LIMIT 1",
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        .map(|json| serde_json::from_str::<Vec<DiagnosticView>>(&json))
        .transpose()
        .map_err(StoreError::corrupt)?
        .unwrap_or_default();
        Ok(warnings
            .into_iter()
            .filter(|warning| {
                [
                    codes::PREVIEW_LIMITS_DEFAULTED,
                    codes::PREVIEW_REPLICAS_CAPPED,
                ]
                .contains(&warning.code.as_str())
            })
            .collect())
    }

    /// Previews against the `[previews]` limits: the installation's count
    /// and the CPU and memory limits of their deployed replicas, and the
    /// count of each `readable` application with previews. Previews being
    /// deleted are not counted, as when creating one.
    ///
    /// # Errors
    /// Returns a storage or decoding error.
    pub async fn preview_usage(&self, readable: &Scope) -> Result<PreviewUsage, StoreError> {
        let rows = sqlx::query!(
            r#"SELECT e.application_id AS "id!",a.name AS "name!",e.resolved_json FROM environments e JOIN applications a ON a.id=e.application_id WHERE e.kind='preview' AND e.delete_intent=0 ORDER BY a.name"#
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?;
        let mut usage = PreviewUsage {
            limits: self.previews.clone(),
            ..PreviewUsage::default()
        };
        for row in rows {
            usage.total += 1;
            let id = ApplicationId::parse(row.id).map_err(StoreError::corrupt)?;
            if readable.contains(&id) {
                match usage.applications.last_mut() {
                    Some(last) if last.id == id => last.previews += 1,
                    _ => usage.applications.push(ApplicationPreviews {
                        id,
                        name: row.name,
                        previews: 1,
                    }),
                }
            }
            let target: Option<ResolvedApplication> = row
                .resolved_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(StoreError::corrupt)?;
            for service in target.iter().flat_map(|target| &target.services) {
                let replicas = u64::from(service.replicas);
                let limits = service.resources.as_ref();
                let cpu = limits.and_then(|limits| limits.cpu_millis).unwrap_or(0);
                let memory = limits.and_then(|limits| limits.memory_bytes).unwrap_or(0);
                usage.cpu_millis += replicas * u64::from(cpu);
                usage.memory_bytes = usage
                    .memory_bytes
                    .saturating_add(replicas.saturating_mul(memory));
            }
        }
        Ok(usage)
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
