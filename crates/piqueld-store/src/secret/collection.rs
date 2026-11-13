//! Garbage collection of secret versions nothing can mount any more. Rotating
//! a secret keeps its old versions, and their Docker secrets, for the
//! deployments that pinned them; once history pruning drops those
//! deployments, the versions go too.
use super::{EnvironmentId, Store, StoreError};
use std::collections::BTreeMap;

impl Store {
    /// The Docker secrets, per environment, of the secret versions and
    /// stored-secret copies nothing can mount any more: not their secret's
    /// current generation, pinned by no deployment (whose prepared targets
    /// name only pinned versions), and named by no running target. Secrets
    /// being deleted are left to their deletion.
    ///
    /// # Errors
    /// Returns storage or decoding errors.
    pub async fn unused_secret_versions(
        &self,
    ) -> Result<BTreeMap<EnvironmentId, Vec<String>>, StoreError> {
        let rows = sqlx::query!(
            r#"WITH targeted(swarm_name) AS (
                SELECT j.value FROM environments e,json_each(e.resolved_json,'$.secret_names') j WHERE e.resolved_json IS NOT NULL
            )
            SELECT v.environment_id AS "environment_id!",v.swarm_name AS "swarm_name!" FROM secret_versions v
            JOIN environment_secrets s ON s.environment_id=v.environment_id AND s.name=v.name
            WHERE v.generation!=s.generation AND s.deletion_id IS NULL
                AND NOT EXISTS(SELECT 1 FROM deployment_secret_pins p WHERE p.environment_id=v.environment_id AND p.name=v.name AND p.generation=v.generation)
                AND v.swarm_name NOT IN (SELECT swarm_name FROM targeted)
            UNION ALL
            SELECT c.environment_id,c.swarm_name FROM application_secret_copies c
            JOIN application_secrets s ON s.application_id=c.application_id AND s.name=c.name
            WHERE c.generation!=s.generation AND s.deletion_id IS NULL
                AND NOT EXISTS(SELECT 1 FROM deployment_stored_secret_pins p WHERE p.environment_id=c.environment_id AND p.name=c.name AND p.generation=c.generation)
                AND c.swarm_name NOT IN (SELECT swarm_name FROM targeted)
            ORDER BY 1,2"#
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?;
        let mut unused = BTreeMap::<_, Vec<_>>::new();
        for row in rows {
            let environment =
                EnvironmentId::parse(row.environment_id).map_err(StoreError::corrupt)?;
            unused.entry(environment).or_default().push(row.swarm_name);
        }
        Ok(unused)
    }

    /// Deletes the versions and copies of `environment` whose Docker secrets
    /// `swarm_names`, which [`Self::unused_secret_versions`] returned, were
    /// removed. A version nothing can mount stays that way: a new deployment
    /// pins only current generations, and a restore only versions its
    /// retained deployment already pins.
    ///
    /// # Errors
    /// Returns storage errors.
    pub async fn forget_secret_versions(
        &self,
        environment: &EnvironmentId,
        swarm_names: &[String],
    ) -> Result<(), StoreError> {
        let (environment, names) = (
            environment.as_str(),
            serde_json::to_string(swarm_names).map_err(StoreError::corrupt)?,
        );
        let (_writer, mut tx) = self.begin_immediate().await?;
        sqlx::query!(
            "DELETE FROM secret_versions WHERE environment_id=?1 AND swarm_name IN (SELECT value FROM json_each(?2))",
            environment,
            names
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        sqlx::query!(
            "DELETE FROM application_secret_copies WHERE environment_id=?1 AND swarm_name IN (SELECT value FROM json_each(?2))",
            environment,
            names
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }

    /// Deletes the stored-secret versions that aren't their secret's current
    /// generation and that no environment has a copy of, so nothing can
    /// mount them.
    ///
    /// # Errors
    /// Returns storage errors.
    pub async fn forget_uncopied_secret_versions(&self) -> Result<(), StoreError> {
        self.write(sqlx::query!(
            "DELETE FROM application_secret_versions WHERE NOT EXISTS(SELECT 1 FROM application_secrets s WHERE s.application_id=application_secret_versions.application_id AND s.name=application_secret_versions.name AND s.generation=application_secret_versions.generation) AND NOT EXISTS(SELECT 1 FROM application_secret_copies c WHERE c.application_id=application_secret_versions.application_id AND c.name=application_secret_versions.name AND c.generation=application_secret_versions.generation)"
        ))
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }
}
