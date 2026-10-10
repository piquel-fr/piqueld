//! Immutable releases, recorded when a tracking environment's preparation
//! succeeds. They belong to the application, so they outlive the environment
//! and deployments that recorded them.
use super::{Store, StoreError, new_id, page_limit};
use piqueld_core::{
    ApplicationId, EnvironmentId, Release, ReleaseId,
    api::{Page, ReleaseView},
    resource::ResolvedApplication,
};
use sqlx::{Sqlite, Transaction};

/// `releases` row shared by the first-page and cursor page queries.
struct ReleaseRow {
    id: String,
    application_id: String,
    content_hash: String,
    release_json: String,
    created_at_ms: i64,
}

impl ReleaseRow {
    fn view(self) -> Result<ReleaseView, StoreError> {
        let release: Release =
            serde_json::from_str(&self.release_json).map_err(StoreError::corrupt)?;
        Ok(ReleaseView {
            id: ReleaseId::parse(self.id).map_err(StoreError::corrupt)?,
            application_id: ApplicationId::parse(self.application_id)
                .map_err(StoreError::corrupt)?,
            content_hash: piqueld_core::Sha256Digest::parse(self.content_hash)
                .map_err(StoreError::corrupt)?,
            created_at_ms: self.created_at_ms,
            fingerprint: release.fingerprint(),
            release,
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
        let id = id.as_str();
        sqlx::query_scalar!(
            "SELECT d.release_id FROM deployments d JOIN operations o ON o.id=d.id WHERE d.environment_id=?1 AND o.promoted=1 ORDER BY d.id DESC LIMIT 1",
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        .flatten()
        .map(|release| ReleaseId::parse(release).map_err(StoreError::corrupt))
        .transpose()
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
        let mut rows = sqlx::query_as!(
            ReleaseRow,
            r#"SELECT id AS "id!",application_id,content_hash,release_json,created_at_ms FROM releases WHERE application_id=?1 AND (?2 IS NULL OR id<?2) ORDER BY id DESC LIMIT ?3"#,
            application,
            before,
            limit_sql
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?;
        let more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = more
            .then(|| rows.last().map(|row| format!("v1:{}", row.id)))
            .flatten();
        Ok(Page {
            items: rows
                .into_iter()
                .map(ReleaseRow::view)
                .collect::<Result<_, _>>()?,
            next_cursor,
        })
    }
}

#[cfg(test)]
mod tests;
