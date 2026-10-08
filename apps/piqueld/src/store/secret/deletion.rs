//! Reserve cleanup under the writer lock, then release it before Docker I/O.
use super::{EnvironmentId, NormalizedApplication, SecretSource, Store, StoreError};
use crate::store::StoredEnvironment;
use piqueld_core::ApplicationId;
use std::collections::{BTreeMap, BTreeSet};

/// A reserved secret deletion: its token and, per environment, the Swarm
/// secret names to remove.
pub(crate) struct SecretDeletion {
    /// Token stored in the secret's `deletion_id`; completion requires it.
    pub(crate) id: String,
    pub(crate) versions: BTreeMap<EnvironmentId, Vec<String>>,
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
    /// currently being deleted from an environment in `scope` or from its
    /// application's store.
    pub(crate) async fn check_secret_references(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        mounted: &BTreeSet<&str>,
        scope: SecretScope<'_>,
    ) -> Result<(), StoreError> {
        let deleting = match scope {
            SecretScope::Application(application) => {
                let id = application.as_str();
                sqlx::query_scalar!(r#"SELECT name AS "name!" FROM environment_secrets WHERE environment_id IN (SELECT id FROM environments WHERE application_id=?1) AND deletion_id IS NOT NULL UNION ALL SELECT name FROM application_secrets WHERE application_id=?1 AND deletion_id IS NOT NULL"#,id)
                    .fetch_all(&mut **tx).await
            }
            SecretScope::Environment(environment) => {
                let id = environment.as_str();
                sqlx::query_scalar!(r#"SELECT name AS "name!" FROM environment_secrets WHERE environment_id=?1 AND deletion_id IS NOT NULL UNION ALL SELECT name FROM application_secrets WHERE application_id=(SELECT application_id FROM environments WHERE id=?1) AND deletion_id IS NOT NULL"#,id)
                    .fetch_all(&mut **tx).await
            }
        }
        .map_err(StoreError::database)?;
        if deleting.iter().any(|name| mounted.contains(name.as_str())) {
            return Err(StoreError::SecretDeleting);
        }
        Ok(())
    }

    /// Whether `environment` still uses its secret `name` from `source`, whose
    /// Docker secrets there are `versions`: its saved configuration mounts it,
    /// its latest deployment captured or pinned it, or its active runtime
    /// target uses one of the versions.
    pub(super) async fn secret_in_use_on(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        environment: &StoredEnvironment,
        name: &str,
        source: SecretSource,
        versions: &[String],
    ) -> Result<bool, StoreError> {
        let id = environment.id().as_str();
        // Saved edits can remove a reference after Deploy captured it but before pinning.
        let captured = sqlx::query_scalar!("SELECT d.manifest_json FROM deployments d JOIN operations o ON o.id=d.id WHERE o.environment_id=?1 AND o.state!='superseded' AND o.id=(SELECT id FROM operations WHERE environment_id=?1 ORDER BY created_at_ms DESC,id DESC LIMIT 1)",id)
            .fetch_optional(&mut **tx).await.map_err(StoreError::database)?;
        let captured = captured
            .map(|json| serde_json::from_str::<Option<NormalizedApplication>>(&json))
            .transpose()
            .map_err(StoreError::corrupt)?
            .flatten();
        let pins = match source {
            SecretSource::Generated => sqlx::query_scalar!("SELECT COUNT(*) FROM deployment_secret_pins p JOIN operations o ON o.id=p.operation_id WHERE p.environment_id=?1 AND p.name=?2 AND o.state!='superseded' AND o.id=(SELECT id FROM operations WHERE environment_id=?1 ORDER BY created_at_ms DESC,id DESC LIMIT 1)",id,name)
                .fetch_one(&mut **tx).await,
            SecretSource::Stored => sqlx::query_scalar!("SELECT COUNT(*) FROM deployment_stored_secret_pins p JOIN operations o ON o.id=p.operation_id WHERE p.environment_id=?1 AND p.name=?2 AND o.state!='superseded' AND o.id=(SELECT id FROM operations WHERE environment_id=?1 ORDER BY created_at_ms DESC,id DESC LIMIT 1)",id,name)
                .fetch_one(&mut **tx).await,
        }
        .map_err(StoreError::database)?;
        let active = environment
            .resolved
            .as_ref()
            .is_some_and(|r| r.secret_names.values().any(|n| versions.contains(n)));
        Ok(environment.manifest().is_some_and(|manifest| {
            manifest
                .mounted_secrets(&environment.environment.target())
                .get(name)
                == Some(&source)
        }) || captured
            .as_ref()
            .is_some_and(|a| a.spec().mounted_secrets().get(name) == Some(&source))
            || pins > 0
            || active)
    }

    /// Reserves an environment's secret for deletion and returns the runtime
    /// versions to remove. A new deletion is refused (`SecretReferenced`)
    /// while the environment still uses it (see `secret_in_use_on`). Resuming
    /// an existing reservation skips that check and reuses its token. `actor`
    /// needs `secrets:write` on the environment's application, checked in the
    /// reserving transaction.
    pub(crate) async fn begin_secret_deletion(
        &self,
        actor: crate::store::Actor<'_>,
        application: &EnvironmentId,
        name: &str,
        expected: i64,
    ) -> Result<SecretDeletion, StoreError> {
        let _writer = self.writers.lock().await;
        let app = self.get(application).await?;
        let id = application.as_str();
        let mut tx = self.pool.begin().await.map_err(StoreError::database)?;
        actor
            .require_on_environment(&mut tx, Self::SECRETS_WRITE, application)
            .await?;
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
        if row.deletion_id.is_none()
            && Self::secret_in_use_on(&mut tx, &app, name, SecretSource::Generated, &versions)
                .await?
        {
            return Err(StoreError::SecretReferenced);
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
            versions: BTreeMap::from([(application.clone(), versions)]),
        })
    }

    /// Deletes the secret, its versions and pins after runtime cleanup succeeded,
    /// recording `secret_deleted` by `actor`. A no-op if the reservation token
    /// no longer matches.
    pub(crate) async fn finish_secret_deletion(
        &self,
        actor: crate::store::Attribution<'_>,
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
            let operator = actor.operator_uid();
            sqlx::query!("INSERT INTO events(application_id,environment_id,kind,message,resource,created_at_ms,actor_user_id,actor_credential_id,actor_operator_uid) VALUES((SELECT application_id FROM environments WHERE id=?1),?1,'secret_deleted','Deleted secret and its runtime versions',?2,?3,?4,?5,?6)",id,name,now,actor.user_id,actor.credential_id,operator)
                .execute(&mut *tx).await.map_err(StoreError::database)?;
        }
        tx.commit().await.map_err(StoreError::database)
    }
}
