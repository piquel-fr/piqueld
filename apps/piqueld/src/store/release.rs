//! Immutable releases, recorded when a tracking environment's preparation
//! succeeds. They belong to the application, so they outlive the environment
//! and deployments that recorded them.
use super::{Store, StoreError, new_id, page_limit};
use piqueld_core::{
    ApplicationId, EnvironmentId, Release, ReleaseId,
    api::{Page, ReleasePromotion, ReleaseView},
    resource::ResolvedApplication,
};
use sqlx::{Sqlite, SqliteConnection, Transaction};

/// `releases` row shared by the first-page and cursor page queries.
struct ReleaseRow {
    id: String,
    application_id: String,
    content_hash: String,
    release_json: String,
    created_at_ms: i64,
}

impl ReleaseRow {
    /// Decodes the release, with every deployment that received it.
    async fn view(self, connection: &mut SqliteConnection) -> Result<ReleaseView, StoreError> {
        let release: Release =
            serde_json::from_str(&self.release_json).map_err(StoreError::corrupt)?;
        let promotions = sqlx::query!(
            r#"SELECT id AS "id!",environment_id,origin_json,created_at_ms FROM deployments WHERE release_id=?1 AND json_extract(origin_json,'$.type')!='build' ORDER BY id DESC"#,
            self.id
        )
        .fetch_all(connection)
        .await
        .map_err(StoreError::database)?
        .into_iter()
        .map(|row| {
            Ok(ReleasePromotion {
                environment_id: EnvironmentId::parse(row.environment_id)
                    .map_err(StoreError::corrupt)?,
                deployment_id: row.id,
                origin: serde_json::from_str(&row.origin_json).map_err(StoreError::corrupt)?,
                created_at_ms: row.created_at_ms,
            })
        })
        .collect::<Result<_, StoreError>>()?;
        Ok(ReleaseView {
            id: ReleaseId::parse(self.id).map_err(StoreError::corrupt)?,
            application_id: ApplicationId::parse(self.application_id)
                .map_err(StoreError::corrupt)?,
            content_hash: piqueld_core::Sha256Digest::parse(self.content_hash)
                .map_err(StoreError::corrupt)?,
            created_at_ms: self.created_at_ms,
            fingerprint: release.fingerprint(),
            release,
            availability: None,
            promotions,
        })
    }
}

impl Store {
    /// Records the release deployment `id`'s prepared `target` runs, when its
    /// environment records releases, and references it from the deployment.
    /// A preparation with the content of one of the application's releases
    /// shares it. Runs in the transaction that saves the prepared target.
    pub(super) async fn record_release_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: &str,
        environment: &EnvironmentId,
        target: &ResolvedApplication,
        now: i64,
    ) -> Result<(), StoreError> {
        let environment = Self::environment_on(tx, environment.as_str())
            .await?
            .ok_or(StoreError::NotFound)?;
        if !environment.environment.kind.records_releases() {
            return Ok(());
        }
        let Some(row) = sqlx::query!(
            "SELECT d.template_json,i.repository_commit FROM deployments d LEFT JOIN deployment_inputs i ON i.operation_id=d.id WHERE d.id=?1",
            id
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)?
        else {
            return Ok(());
        };
        let template = serde_json::from_str(&row.template_json).map_err(StoreError::corrupt)?;
        let release = Release::new(template, row.repository_commit, target);
        let hash = release.content_hash();
        let (hash, json) = (
            hash.as_str(),
            serde_json::to_string(&release).map_err(StoreError::corrupt)?,
        );
        let (release_id, application) = (
            new_id("rel"),
            environment.environment.application_id.as_str(),
        );
        sqlx::query!(
            "INSERT INTO releases(id,application_id,content_hash,release_json,created_at_ms) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(application_id,content_hash) DO NOTHING",
            release_id,
            application,
            hash,
            json,
            now
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        sqlx::query!(
            "UPDATE deployments SET release_id=(SELECT id FROM releases WHERE application_id=?1 AND content_hash=?2) WHERE id=?3",
            application,
            hash,
            id
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// Records releases for deployments prepared before releases existed
    /// (migration `0022_releases.sql`), oldest first, so their environments
    /// show what they run. Runs on every start: once recorded, only
    /// deployments of environments that record no releases match, and
    /// deployments whose records no longer decode, which are logged and skipped.
    pub(super) async fn record_missing_releases(&self) -> Result<(), StoreError> {
        let prepared = sqlx::query!(
            r#"SELECT d.id AS "id!",d.environment_id,d.created_at_ms,o.target_json AS "target_json!" FROM deployments d JOIN operations o ON o.id=d.id WHERE d.release_id IS NULL AND o.target_json IS NOT NULL ORDER BY d.created_at_ms,d.id"#
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?;
        for row in prepared {
            let recorded = async {
                let target = serde_json::from_str(&row.target_json).map_err(StoreError::corrupt)?;
                let environment =
                    EnvironmentId::parse(row.environment_id).map_err(StoreError::corrupt)?;
                let (_writer, mut tx) = self.begin_immediate().await?;
                Self::record_release_on(&mut tx, &row.id, &environment, &target, row.created_at_ms)
                    .await?;
                tx.commit().await.map_err(StoreError::database)
            }
            .await;
            if let Err(error) = recorded {
                tracing::warn!(deployment_id = %row.id, %error, "deployment release not recorded");
            }
        }
        Ok(())
    }

    /// The release environment `id`'s current runtime target runs, if any.
    /// # Errors
    /// Returns storage or decoding errors.
    pub async fn current_release(
        &self,
        id: &EnvironmentId,
    ) -> Result<Option<ReleaseId>, StoreError> {
        Ok(self
            .current_deployment(id)
            .await?
            .and_then(|current| current.release))
    }

    /// Lists an application's releases, newest first.
    /// # Errors
    /// Returns storage, decoding, absence, or invalid cursor errors.
    pub async fn releases(
        &self,
        application: &ApplicationId,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<ReleaseView>, StoreError> {
        self.application(application).await?;
        let limit_sql = page_limit(limit)? + 1;
        let before = cursor
            .map(|v| v.strip_prefix("v1:").ok_or(StoreError::InvalidInput))
            .transpose()?;
        let application = application.as_str();
        let mut snapshot = self.pool.begin().await.map_err(StoreError::database)?;
        let mut rows = sqlx::query_as!(
            ReleaseRow,
            r#"SELECT id AS "id!",application_id,content_hash,release_json,created_at_ms FROM releases WHERE application_id=?1 AND (?2 IS NULL OR id<?2) ORDER BY id DESC LIMIT ?3"#,
            application,
            before,
            limit_sql
        )
        .fetch_all(&mut *snapshot)
        .await
        .map_err(StoreError::database)?;
        let more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = more
            .then(|| rows.last().map(|row| format!("v1:{}", row.id)))
            .flatten();
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            items.push(row.view(&mut snapshot).await?);
        }
        Ok(Page { items, next_cursor })
    }

    /// Reads release `id` of `application`; `NotFound` for another's.
    /// # Errors
    /// Returns storage, decoding, or absence errors.
    pub async fn release(
        &self,
        application: &ApplicationId,
        id: &ReleaseId,
    ) -> Result<ReleaseView, StoreError> {
        let mut snapshot = self.pool.begin().await.map_err(StoreError::database)?;
        let (application, id) = (application.as_str(), id.as_str());
        sqlx::query_as!(
            ReleaseRow,
            r#"SELECT id AS "id!",application_id,content_hash,release_json,created_at_ms FROM releases WHERE id=?1 AND application_id=?2"#,
            id,
            application
        )
        .fetch_optional(&mut *snapshot)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?
        .view(&mut snapshot)
        .await
    }
}

#[cfg(test)]
mod tests;
