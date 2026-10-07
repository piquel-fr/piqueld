//! Reserve cleanup under the writer lock, then release it before Docker I/O.
use super::{EnvironmentId, NormalizedApplication, Store, StoreError};
use piqueld_core::ApplicationId;
use std::collections::BTreeSet;

/// A reserved secret deletion: its token and the Swarm secret names to remove.
pub(crate) struct SecretDeletion {
    /// Token stored in `environment_secrets.deletion_id`; completion requires it.
    pub(crate) id: String,
    pub(crate) versions: Vec<String>,
}

/// Environments whose pending secret deletions a manifest must not reference.
pub(crate) enum SecretScope<'a> {
    /// Every environment of the application: saved configuration is shared by
    /// all of them.
    Application(&'a ApplicationId),
    /// The one environment a deployment prepares.
    Environment(&'a EnvironmentId),
}

impl Store {
    /// Rejects a manifest mounting `mounted` secrets when one of them is
    /// currently being deleted from an environment in `scope`.
    pub(crate) async fn check_secret_references(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        mounted: &BTreeSet<&str>,
        scope: SecretScope<'_>,
    ) -> Result<(), StoreError> {
        let deleting = match scope {
            SecretScope::Application(application) => {
                let id = application.as_str();
                sqlx::query_scalar!("SELECT name FROM environment_secrets WHERE environment_id IN (SELECT id FROM environments WHERE application_id=?1) AND deletion_id IS NOT NULL",id)
                    .fetch_all(&mut **tx).await
            }
            SecretScope::Environment(environment) => {
                let id = environment.as_str();
                sqlx::query_scalar!("SELECT name FROM environment_secrets WHERE environment_id=?1 AND deletion_id IS NOT NULL",id)
                    .fetch_all(&mut **tx).await
            }
        }
        .map_err(StoreError::database)?;
        if deleting.iter().any(|name| mounted.contains(name.as_str())) {
            return Err(StoreError::SecretDeleting);
        }
        Ok(())
    }

    /// Reserves a secret for deletion and returns the runtime versions to remove.
    /// A new deletion is refused (`SecretReferenced`) while the saved
    /// configuration, the latest deployment's manifest or pins, or the active
    /// runtime target still use the secret. Resuming an existing reservation
    /// skips those checks and reuses its token.
    pub(crate) async fn begin_secret_deletion(
        &self,
        application: &EnvironmentId,
        name: &str,
        expected: i64,
    ) -> Result<SecretDeletion, StoreError> {
        let _writer = self.writers.lock().await;
        let app = self.get(application).await?;
        let id = application.as_str();
        let mut tx = self.pool.begin().await.map_err(StoreError::database)?;
        let row = sqlx::query!("SELECT generation,deletion_id FROM environment_secrets WHERE environment_id=?1 AND name=?2",id,name)
            .fetch_optional(&mut *tx).await.map_err(StoreError::database)?.ok_or(StoreError::NotFound)?;
        Self::secret_version_matches(expected, row.generation)?;
        let versions = sqlx::query_scalar!(
            "SELECT swarm_name FROM secret_versions WHERE environment_id=?1 AND name=?2",
            id,
            name
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        if row.deletion_id.is_none() {
            // Saved edits can remove a reference after Deploy captured it but before pinning.
            let captured = sqlx::query_scalar!("SELECT d.manifest_json FROM deployments d JOIN operations o ON o.id=d.id WHERE o.environment_id=?1 AND o.state!='superseded' AND o.id=(SELECT id FROM operations WHERE environment_id=?1 ORDER BY created_at_ms DESC,id DESC LIMIT 1)",id)
                .fetch_optional(&mut *tx).await.map_err(StoreError::database)?;
            let captured = captured
                .map(|json| serde_json::from_str::<Option<NormalizedApplication>>(&json))
                .transpose()
                .map_err(StoreError::corrupt)?
                .flatten();
            let pins = sqlx::query_scalar!("SELECT COUNT(*) FROM deployment_secret_pins p JOIN operations o ON o.id=p.operation_id WHERE p.environment_id=?1 AND p.name=?2 AND o.state!='superseded' AND o.id=(SELECT id FROM operations WHERE environment_id=?1 ORDER BY created_at_ms DESC,id DESC LIMIT 1)",id,name)
                .fetch_one(&mut *tx).await.map_err(StoreError::database)?;
            let active = app
                .resolved
                .as_ref()
                .is_some_and(|r| r.secret_names.values().any(|n| versions.contains(n)));
            if app
                .manifest()
                .is_some_and(|manifest| manifest.spec().mounted_secret_names().contains(name))
                || captured
                    .as_ref()
                    .is_some_and(|a| a.spec().mounted_secret_names().contains(name))
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
            "UPDATE environment_secrets SET deletion_id=?3 WHERE environment_id=?1 AND name=?2",
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

    /// Deletes the secret, its versions and pins after runtime cleanup succeeded,
    /// recording `secret_deleted`. A no-op if the reservation token no longer matches.
    pub(crate) async fn finish_secret_deletion(
        &self,
        application: &EnvironmentId,
        name: &str,
        deletion_id: &str,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let id = application.as_str();
        // Concurrent retries may finish after a new secret with the same name was created.
        // The reservation token prevents those completions from touching the new value.
        sqlx::query!("DELETE FROM deployment_secret_pins WHERE environment_id=?1 AND name=?2 AND EXISTS(SELECT 1 FROM environment_secrets WHERE environment_id=?1 AND name=?2 AND deletion_id=?3)",id,name,deletion_id)
            .execute(&mut *tx).await.map_err(StoreError::database)?;
        let deleted = sqlx::query!("DELETE FROM environment_secrets WHERE environment_id=?1 AND name=?2 AND deletion_id=?3",id,name,deletion_id)
            .execute(&mut *tx).await.map_err(StoreError::database)?.rows_affected();
        if deleted > 0 {
            let now = super::now_ms();
            sqlx::query!("INSERT INTO events(application_id,environment_id,kind,message,resource,created_at_ms) VALUES((SELECT application_id FROM environments WHERE id=?1),?1,'secret_deleted','Deleted secret and its runtime versions',?2,?3)",id,name,now)
                .execute(&mut *tx).await.map_err(StoreError::database)?;
        }
        tx.commit().await.map_err(StoreError::database)
    }
}
