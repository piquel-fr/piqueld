//! Application-scoped encrypted values and durable deployment version pins.
mod deletion;
mod key;

use super::{ApplicationId, NormalizedApplication, Store, StoreError, now_ms};
use crate::secrets::{Envelope, SecretCipher};
use piqueld_core::api::SecretMetadata;
use std::collections::{BTreeMap, BTreeSet};
use zeroize::Zeroizing;

impl Store {
    /// Lists metadata without accessing the master key or decrypting values.
    /// # Errors
    /// Returns application absence or database errors.
    pub async fn secrets(
        &self,
        application: &ApplicationId,
    ) -> Result<Vec<SecretMetadata>, StoreError> {
        self.get(application).await?;
        let id = application.as_str();
        let rows=sqlx::query!("SELECT s.name,s.generation,s.updated_at_ms,s.deletion_id,v.available FROM application_secrets s JOIN secret_versions v USING(application_id,name,generation) WHERE s.application_id=?1 ORDER BY s.name",id).fetch_all(&self.pool).await.map_err(StoreError::database)?;
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
    /// # Errors
    /// Returns invalid input, version conflict, key or database errors.
    pub async fn put_secret(
        &self,
        application: &ApplicationId,
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
        if app.delete_intent {
            return Err(StoreError::Busy);
        }
        let id = application.as_str();
        let mut tx = self.pool.begin().await.map_err(StoreError::database)?;
        let existing = sqlx::query!(
            "SELECT generation,deletion_id FROM application_secrets WHERE application_id=?1 AND name=?2", id, name
        ).fetch_optional(&mut *tx).await.map_err(StoreError::database)?;
        let current = existing.as_ref().map_or(0, |row| row.generation);
        if existing.is_some_and(|row| row.deletion_id.is_some()) {
            return Err(StoreError::SecretDeleting);
        }
        Self::secret_version_matches(expected, current)?;
        if current == 0 {
            let count = sqlx::query_scalar!(
                "SELECT COUNT(*) FROM application_secrets WHERE application_id=?1",
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
        sqlx::query!("INSERT INTO application_secrets(application_id,name,generation,updated_at_ms) VALUES(?1,?2,?3,?4) ON CONFLICT(application_id,name) DO UPDATE SET generation=excluded.generation,updated_at_ms=excluded.updated_at_ms",id,name,generation,now).execute(&mut *tx).await.map_err(StoreError::database)?;
        sqlx::query!("INSERT INTO secret_versions(application_id,name,generation,swarm_name,nonce,ciphertext) VALUES(?1,?2,?3,?4,?5,?6)",id,name,generation,swarm_name,envelope.nonce,envelope.ciphertext).execute(&mut *tx).await.map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(SecretMetadata {
            name: name.into(),
            generation,
            updated_at_ms: now,
            deleting: false,
            unavailable: false,
        })
    }
    fn secret_version_matches(expected: i64, actual: i64) -> Result<(), StoreError> {
        if expected == actual {
            Ok(())
        } else {
            Err(StoreError::SecretVersionConflict { expected, actual })
        }
    }
    pub(crate) async fn pin_secrets(
        &self,
        operation: &str,
        app: &NormalizedApplication,
    ) -> Result<BTreeMap<String, String>, StoreError> {
        let _writer = self.writers.lock().await;
        let mut tx = self.pool.begin().await.map_err(StoreError::database)?;
        let pins = Self::pin_secrets_on(&mut tx, operation, app).await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(pins)
    }
    pub(super) async fn pin_secrets_on(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        operation: &str,
        app: &NormalizedApplication,
    ) -> Result<BTreeMap<String, String>, StoreError> {
        Self::check_secret_references(tx, app).await?;
        let prepared = sqlx::query_scalar!(
            "SELECT operation_id FROM deployment_secrets_prepared WHERE operation_id=?1",
            operation
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)?
        .is_some();
        let id = app.id().as_str();
        if !prepared {
            let names = app
                .spec()
                .services
                .iter()
                .flat_map(|s| s.secrets.iter().map(|s| s.name.as_str()))
                .collect::<BTreeSet<_>>();
            let mut unavailable = Vec::new();
            for name in &names {
                let available = sqlx::query_scalar!("SELECT v.available FROM application_secrets s JOIN secret_versions v USING(application_id,name,generation) WHERE s.application_id=?1 AND s.name=?2",id,name)
                    .fetch_optional(&mut **tx).await.map_err(StoreError::database)?;
                if available != Some(1) {
                    unavailable.push(*name);
                }
            }
            if !unavailable.is_empty() {
                return Err(StoreError::SecretUnavailable {
                    names: unavailable.join(", "),
                });
            }
            for name in names {
                let changed=sqlx::query!("INSERT INTO deployment_secret_pins(operation_id,application_id,name,generation) SELECT ?1,application_id,name,generation FROM application_secrets WHERE application_id=?2 AND name=?3",operation,id,name).execute(&mut **tx).await.map_err(StoreError::database)?.rows_affected();
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
        let rows=sqlx::query!("SELECT p.name,v.swarm_name,v.available FROM deployment_secret_pins p JOIN secret_versions v USING(application_id,name,generation) WHERE p.operation_id=?1",operation).fetch_all(&mut **tx).await.map_err(StoreError::database)?;
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
    pub(crate) async fn secret_plaintext(
        &self,
        application: &ApplicationId,
        swarm_name: &str,
    ) -> Result<Zeroizing<Vec<u8>>, StoreError> {
        let id = application.as_str();
        let (_writer, mut tx) = self.begin_immediate().await?;
        let row=sqlx::query!("SELECT name,generation,nonce,ciphertext,available FROM secret_versions WHERE application_id=?1 AND swarm_name=?2",id,swarm_name).fetch_optional(&mut *tx).await.map_err(StoreError::database)?.ok_or(StoreError::NotFound)?;
        if row.available == 0 {
            return Err(StoreError::SecretUnavailable { names: row.name });
        }
        let cipher = self.verified_secret_cipher(&mut tx).await?;
        let plaintext = cipher
            .decrypt(
                id,
                &row.name,
                row.generation,
                &Envelope {
                    nonce: row.nonce,
                    ciphertext: row.ciphertext,
                },
            )
            .map_err(StoreError::SecretSource)?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(plaintext)
    }

    /// Authenticate all target values before publishing it or mutating Docker.
    pub(crate) async fn check_target_secrets(
        &self,
        target: &piqueld_core::ResolvedApplication,
    ) -> Result<(), StoreError> {
        for swarm_name in target.secret_names.values() {
            self.secret_plaintext(&target.id, swarm_name).await?;
        }
        Ok(())
    }

    pub(crate) async fn secret_names(
        &self,
        application: &ApplicationId,
    ) -> Result<Vec<String>, StoreError> {
        let id = application.as_str();
        sqlx::query_scalar!(
            "SELECT swarm_name FROM secret_versions WHERE application_id=?1",
            id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)
    }
    async fn check_secret_quota(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        id: &str,
        incoming: usize,
    ) -> Result<(), StoreError> {
        let usage = sqlx::query!("SELECT COUNT(*) AS versions, COALESCE(SUM(length(ciphertext)),0) AS bytes FROM secret_versions WHERE application_id=?1 AND available=1",id)
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
