//! Environment-scoped encrypted values and durable deployment version pins.
mod deletion;
pub(super) use deletion::SecretScope;
mod key;

use super::{EnvironmentId, NormalizedApplication, Operation, Store, StoreError, now_ms};
use crate::secrets::{Envelope, Generate, SecretCipher};
use anyhow::Context;
use piqueld_core::api::SecretMetadata;
use std::collections::BTreeMap;
use zeroize::Zeroizing;

impl Store {
    /// Permission for storing and deleting secret values.
    const SECRETS_WRITE: piqueld_core::access::AppPermission =
        piqueld_core::access::AppPermission::SecretsWrite;

    /// Lists metadata without accessing the master key or decrypting values.
    /// # Errors
    /// Returns application absence or database errors.
    pub async fn secrets(
        &self,
        application: &EnvironmentId,
    ) -> Result<Vec<SecretMetadata>, StoreError> {
        self.get(application).await?;
        let id = application.as_str();
        let rows=sqlx::query!("SELECT s.name,s.generation,s.updated_at_ms,s.deletion_id,v.available FROM environment_secrets s JOIN secret_versions v USING(environment_id,name,generation) WHERE s.environment_id=?1 ORDER BY s.name",id).fetch_all(&self.pool).await.map_err(StoreError::database)?;
        Ok(rows
            .into_iter()
            .map(|r| SecretMetadata {
                name: r.name,
                generation: r.generation,
                updated_at_ms: r.updated_at_ms,
                deleting: r.deletion_id.is_some(),
                unavailable: r.available == 0,
            })
            .collect())
    }
    /// Creates or replaces a logical secret. Rotation only affects a later deployment.
    /// `expected` is the current generation (zero to create). The value is
    /// encrypted into a new immutable version with its own Swarm secret name
    /// (`piqueld-secret-<uuid>`), subject to per-environment count and byte quotas.
    /// Rejected while the environment or the secret is being deleted. `actor`
    /// needs `secrets:write` on the environment's application, checked in the
    /// transaction.
    ///
    /// # Errors
    /// Returns invalid input, refusal, version conflict, key or database errors.
    pub async fn put_secret(
        &self,
        actor: super::Actor<'_>,
        application: &EnvironmentId,
        name: &str,
        expected: i64,
        value: Vec<u8>,
    ) -> Result<SecretMetadata, StoreError> {
        let value = Zeroizing::new(value);
        if !piqueld_core::resource::valid_logical_name(name)
            || value.is_empty()
            || value.len() > 500 * 1024
            || expected < 0
        {
            return Err(StoreError::InvalidInput);
        }
        let _writer = self.writers.lock().await;
        let app = self.get(application).await?;
        if app.delete_intent() {
            return Err(StoreError::Busy);
        }
        let id = application.as_str();
        let mut tx = self.pool.begin().await.map_err(StoreError::database)?;
        actor
            .require_on_environment(&mut tx, Self::SECRETS_WRITE, application)
            .await?;
        let existing = sqlx::query!(
            "SELECT generation,deletion_id FROM environment_secrets WHERE environment_id=?1 AND name=?2", id, name
        ).fetch_optional(&mut *tx).await.map_err(StoreError::database)?;
        let current = existing.as_ref().map_or(0, |row| row.generation);
        if existing.is_some_and(|row| row.deletion_id.is_some()) {
            return Err(StoreError::SecretDeleting);
        }
        Self::secret_version_matches(expected, current)?;
        if current == 0 {
            let count = sqlx::query_scalar!(
                "SELECT COUNT(*) FROM environment_secrets WHERE environment_id=?1",
                id
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(StoreError::database)?;
            if count >= 100 {
                return Err(StoreError::InvalidInput);
            }
        }
        let generation = current.checked_add(1).ok_or(StoreError::InvalidInput)?;
        Self::check_secret_quota(&mut tx, id, value.len()).await?;
        let cipher = self.verified_secret_cipher(&mut tx).await?;
        let envelope = cipher
            .encrypt(id, name, generation, &value)
            .map_err(StoreError::SecretSource)?;
        let now = now_ms();
        let swarm_name = format!("piqueld-secret-{}", uuid::Uuid::now_v7().simple());
        sqlx::query!("INSERT INTO environment_secrets(environment_id,name,generation,updated_at_ms) VALUES(?1,?2,?3,?4) ON CONFLICT(environment_id,name) DO UPDATE SET generation=excluded.generation,updated_at_ms=excluded.updated_at_ms",id,name,generation,now).execute(&mut *tx).await.map_err(StoreError::database)?;
        sqlx::query!("INSERT INTO secret_versions(environment_id,name,generation,swarm_name,nonce,ciphertext) VALUES(?1,?2,?3,?4,?5,?6)",id,name,generation,swarm_name,envelope.nonce,envelope.ciphertext).execute(&mut *tx).await.map_err(StoreError::database)?;
        // History records the logical name and version, never the value.
        let message = format!("Stored secret version {generation}");
        let by = actor.attribution();
        let operator = by.operator_uid();
        sqlx::query!("INSERT INTO events(application_id,environment_id,kind,message,resource,created_at_ms,actor_user_id,actor_credential_id,actor_operator_uid) VALUES((SELECT application_id FROM environments WHERE id=?1),?1,'secret_saved',?2,?3,?4,?5,?6,?7)",id,message,name,now,by.user_id,by.credential_id,operator).execute(&mut *tx).await.map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(SecretMetadata {
            name: name.into(),
            generation,
            updated_at_ms: now,
            deleting: false,
            unavailable: false,
        })
    }
    /// Optimistic concurrency check against the secret's current generation.
    fn secret_version_matches(expected: i64, actual: i64) -> Result<(), StoreError> {
        if expected == actual {
            Ok(())
        } else {
            Err(StoreError::SecretVersionConflict { expected, actual })
        }
    }
    /// Stores values for declared secrets that a service mounts and that have
    /// none; unmounted declarations wait until a service needs them. A stored
    /// value, generated or set manually, is never replaced, so deploys never
    /// rotate it.
    /// Call before pinning, outside the writer lock: RSA generation takes time.
    /// Each value is stored as generation 1 with `put_secret`; losing a race to
    /// a concurrent write keeps the other value.
    ///
    /// # Errors
    /// Returns `SecretSource` when generation fails, and other `put_secret`
    /// errors (count or byte quotas, application deletion, key or database
    /// errors) unchanged.
    pub(crate) async fn generate_secrets(
        &self,
        id: &EnvironmentId,
        app: &NormalizedApplication,
    ) -> Result<(), StoreError> {
        let id_str = id.as_str();
        let existing = sqlx::query_scalar!(
            "SELECT name FROM environment_secrets WHERE environment_id=?1",
            id_str
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?;
        let mounted = app.spec().mounted_secret_names();
        for secret in &app.spec().secrets {
            if existing.contains(&secret.name) || !mounted.contains(secret.name.as_str()) {
                continue;
            }
            let generator = secret.generate.clone();
            let mut value = tokio::task::spawn_blocking(move || generator.generate())
                .await
                .map_err(anyhow::Error::from)
                .and_then(|value| value)
                .with_context(|| format!("generate secret {}", secret.name))
                .map_err(StoreError::SecretSource)?;
            match self
                .put_secret(
                    super::Actor::Daemon,
                    id,
                    &secret.name,
                    0,
                    std::mem::take(&mut *value),
                )
                .await
            {
                // A value set concurrently wins over the generated one.
                Ok(_) | Err(StoreError::SecretVersionConflict { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
    /// Generates values for mounted declared secrets that have none (see
    /// `generate_secrets`), then pins the secret versions a deployment uses in
    /// its own transaction; see `pin_secrets_on`.
    pub(crate) async fn pin_secrets(
        &self,
        operation: &Operation,
        app: &NormalizedApplication,
    ) -> Result<BTreeMap<String, String>, StoreError> {
        self.generate_secrets(&operation.environment_id, app)
            .await?;
        let _writer = self.writers.lock().await;
        let mut tx = self.pool.begin().await.map_err(StoreError::database)?;
        let pins = Self::pin_secrets_on(&mut tx, operation, app).await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(pins)
    }
    /// Returns the logical-to-Swarm secret names a deployment operation uses.
    /// The first call pins the current generation of every referenced secret, so
    /// retries deploy the same versions even after rotation; later calls reuse
    /// the pins. Fails if a referenced secret is missing (`InvalidInput`), being
    /// deleted, or had its value discarded by key recovery.
    pub(super) async fn pin_secrets_on(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        operation: &Operation,
        app: &NormalizedApplication,
    ) -> Result<BTreeMap<String, String>, StoreError> {
        Self::check_secret_references(
            tx,
            &app.spec().mounted_secret_names(),
            SecretScope::Environment(&operation.environment_id),
        )
        .await?;
        let (operation, id) = (operation.id.as_str(), operation.environment_id.as_str());
        let prepared = sqlx::query_scalar!(
            "SELECT operation_id FROM deployment_secrets_prepared WHERE operation_id=?1",
            operation
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)?
        .is_some();
        if !prepared {
            for name in app.spec().mounted_secret_names() {
                let changed=sqlx::query!("INSERT INTO deployment_secret_pins(operation_id,environment_id,name,generation) SELECT ?1,environment_id,name,generation FROM environment_secrets WHERE environment_id=?2 AND name=?3",operation,id,name).execute(&mut **tx).await.map_err(StoreError::database)?.rows_affected();
                if changed != 1 {
                    return Err(StoreError::InvalidInput);
                }
            }
            sqlx::query!(
                "INSERT INTO deployment_secrets_prepared(operation_id) VALUES(?1)",
                operation
            )
            .execute(&mut **tx)
            .await
            .map_err(StoreError::database)?;
        }
        let rows=sqlx::query!("SELECT p.name,v.swarm_name,v.available FROM deployment_secret_pins p JOIN secret_versions v USING(environment_id,name,generation) WHERE p.operation_id=?1",operation).fetch_all(&mut **tx).await.map_err(StoreError::database)?;
        let unavailable = rows
            .iter()
            .filter(|r| r.available == 0)
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>();
        if !unavailable.is_empty() {
            return Err(StoreError::SecretUnavailable {
                names: unavailable.join(", "),
            });
        }
        Ok(rows.into_iter().map(|r| (r.name, r.swarm_name)).collect())
    }
    /// Decrypts one version by its Swarm secret name, for creating the runtime secret.
    pub(crate) async fn secret_plaintext(
        &self,
        application: &EnvironmentId,
        swarm_name: &str,
    ) -> Result<Zeroizing<Vec<u8>>, StoreError> {
        let id = application.as_str();
        let row=sqlx::query!("SELECT name,generation,nonce,ciphertext,available FROM secret_versions WHERE environment_id=?1 AND swarm_name=?2",id,swarm_name).fetch_optional(&self.pool).await.map_err(StoreError::database)?.ok_or(StoreError::NotFound)?;
        if row.available == 0 {
            return Err(StoreError::SecretUnavailable { names: row.name });
        }
        let cipher =
            SecretCipher::load(&self.secret_key_path, true).map_err(StoreError::SecretSource)?;
        cipher
            .decrypt(
                id,
                &row.name,
                row.generation,
                &Envelope {
                    nonce: row.nonce,
                    ciphertext: row.ciphertext,
                },
            )
            .map_err(StoreError::SecretSource)
    }
    /// Swarm secret names of every stored version, used to clean up after environment deletion.
    pub(crate) async fn secret_names(
        &self,
        application: &EnvironmentId,
    ) -> Result<Vec<String>, StoreError> {
        let id = application.as_str();
        sqlx::query_scalar!(
            "SELECT swarm_name FROM secret_versions WHERE environment_id=?1",
            id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)
    }
    /// Enforces the per-environment limits of 1000 available versions and
    /// 100 MiB of ciphertext, including the incoming value.
    async fn check_secret_quota(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        id: &str,
        incoming: usize,
    ) -> Result<(), StoreError> {
        let usage = sqlx::query!("SELECT COUNT(*) AS versions, COALESCE(SUM(length(ciphertext)),0) AS bytes FROM secret_versions WHERE environment_id=?1 AND available=1",id)
            .fetch_one(&mut **tx).await.map_err(StoreError::database)?;
        // Include the 16-byte authentication tag in the persisted-byte limit.
        if usage.versions >= 1000
            || usage.bytes + i64::try_from(incoming + 16).map_err(StoreError::invalid_input)?
                > 100 * 1024 * 1024
        {
            return Err(StoreError::SecretQuota);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
