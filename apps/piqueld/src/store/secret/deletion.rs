//! Reserve cleanup under the writer lock, then release it before Docker I/O.
use super::{ApplicationId, NormalizedApplication, Store, StoreError};

pub(crate) struct SecretDeletion {
    pub(crate) id: String,
    pub(crate) versions: Vec<String>,
}

impl Store {
    pub(crate) async fn check_secret_references(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        app: &NormalizedApplication,
    ) -> Result<(), StoreError> {
        let id = app.id().as_str();
        let deleting = sqlx::query_scalar!("SELECT name FROM application_secrets WHERE application_id=?1 AND deletion_id IS NOT NULL",id)
            .fetch_all(&mut **tx).await.map_err(StoreError::database)?;
        if deleting
            .iter()
            .any(|name| Self::references_secret(app, name))
        {
            return Err(StoreError::SecretDeleting);
        }
        Ok(())
    }

    fn references_secret(app: &NormalizedApplication, name: &str) -> bool {
        app.spec()
            .services
            .iter()
            .any(|s| s.secrets.iter().any(|s| s.name == name))
    }

    pub(crate) async fn begin_secret_deletion(
        &self,
        application: &ApplicationId,
        name: &str,
        expected: i64,
    ) -> Result<SecretDeletion, StoreError> {
        let _writer = self.writers.lock().await;
        let app = self.get(application).await?;
        let id = application.as_str();
        let mut tx = self.pool.begin().await.map_err(StoreError::database)?;
        let row = sqlx::query!("SELECT generation,deletion_id FROM application_secrets WHERE application_id=?1 AND name=?2",id,name)
            .fetch_optional(&mut *tx).await.map_err(StoreError::database)?.ok_or(StoreError::NotFound)?;
        Self::secret_version_matches(expected, row.generation)?;
        let versions = sqlx::query_scalar!(
            "SELECT swarm_name FROM secret_versions WHERE application_id=?1 AND name=?2",
            id,
            name
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        if row.deletion_id.is_none() {
            // Saved edits can remove a reference after Deploy captured it but before pinning.
            let captured = sqlx::query_scalar!("SELECT d.manifest_json FROM deployments d JOIN operations o ON o.id=d.id WHERE o.application_id=?1 AND o.state!='superseded' AND o.id=(SELECT id FROM operations WHERE application_id=?1 ORDER BY created_at_ms DESC,id DESC LIMIT 1)",id)
                .fetch_optional(&mut *tx).await.map_err(StoreError::database)?;
            let captured = captured
                .map(|json| serde_json::from_str::<NormalizedApplication>(&json))
                .transpose()
                .map_err(StoreError::corrupt)?;
            let pins = sqlx::query_scalar!("SELECT COUNT(*) FROM deployment_secret_pins p JOIN operations o ON o.id=p.operation_id WHERE p.application_id=?1 AND p.name=?2 AND o.state!='superseded' AND o.id=(SELECT id FROM operations WHERE application_id=?1 ORDER BY created_at_ms DESC,id DESC LIMIT 1)",id,name)
                .fetch_one(&mut *tx).await.map_err(StoreError::database)?;
            let active = app
                .resolved
                .as_ref()
                .is_some_and(|r| r.secret_names.values().any(|n| versions.contains(n)));
            if Self::references_secret(&app.application, name)
                || captured
                    .as_ref()
                    .is_some_and(|a| Self::references_secret(a, name))
                || pins > 0
                || active
            {
                return Err(StoreError::SecretReferenced);
            }
        }
        let deletion_id = row
            .deletion_id
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        sqlx::query!(
            "UPDATE application_secrets SET deletion_id=?3 WHERE application_id=?1 AND name=?2",
            id,
            name,
            deletion_id
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(SecretDeletion {
            id: deletion_id,
            versions,
        })
    }

    pub(crate) async fn finish_secret_deletion(
        &self,
        application: &ApplicationId,
        name: &str,
        deletion_id: &str,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let id = application.as_str();
        // Concurrent retries may finish after a new secret with the same name was created.
        // The reservation token prevents those completions from touching the new value.
        sqlx::query!("DELETE FROM deployment_secret_pins WHERE application_id=?1 AND name=?2 AND EXISTS(SELECT 1 FROM application_secrets WHERE application_id=?1 AND name=?2 AND deletion_id=?3)",id,name,deletion_id)
            .execute(&mut *tx).await.map_err(StoreError::database)?;
        sqlx::query!("DELETE FROM application_secrets WHERE application_id=?1 AND name=?2 AND deletion_id=?3",id,name,deletion_id)
            .execute(&mut *tx).await.map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }
}
