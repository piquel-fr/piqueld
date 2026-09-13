//! Application-scoped encrypted values and durable deployment version pins.
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
        let rows=sqlx::query!("SELECT name,generation,updated_at_ms FROM application_secrets WHERE application_id=?1 ORDER BY name",id).fetch_all(&self.pool).await.map_err(StoreError::database)?;
        Ok(rows
            .into_iter()
            .map(|r| SecretMetadata {
                name: r.name,
                generation: r.generation,
                updated_at_ms: r.updated_at_ms,
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
        let current = sqlx::query_scalar!(
            "SELECT generation FROM application_secrets WHERE application_id=?1 AND name=?2",
            id,
            name
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .unwrap_or(0);
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
        let exists = sqlx::query_scalar!("SELECT COUNT(*) FROM secret_versions")
            .fetch_one(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        let cipher = SecretCipher::load(&self.secret_key_path, exists > 0)
            .map_err(StoreError::SecretSource)?;
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
        let prepared = sqlx::query_scalar!(
            "SELECT operation_id FROM deployment_secrets_prepared WHERE operation_id=?1",
            operation
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)?
        .is_some();
        let id = app.id.as_str();
        if !prepared {
            let names = app
                .spec
                .services
                .iter()
                .flat_map(|s| s.secrets.iter().map(|s| s.name.as_str()))
                .collect::<BTreeSet<_>>();
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
        let rows=sqlx::query!("SELECT p.name,v.swarm_name FROM deployment_secret_pins p JOIN secret_versions v USING(application_id,name,generation) WHERE p.operation_id=?1",operation).fetch_all(&mut **tx).await.map_err(StoreError::database)?;
        Ok(rows.into_iter().map(|r| (r.name, r.swarm_name)).collect())
    }
    pub(crate) async fn secret_plaintext(
        &self,
        application: &ApplicationId,
        swarm_name: &str,
    ) -> Result<Zeroizing<Vec<u8>>, StoreError> {
        let id = application.as_str();
        let row=sqlx::query!("SELECT name,generation,nonce,ciphertext FROM secret_versions WHERE application_id=?1 AND swarm_name=?2",id,swarm_name).fetch_optional(&self.pool).await.map_err(StoreError::database)?.ok_or(StoreError::NotFound)?;
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
    pub(crate) async fn secret_deletion_guard(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.writers.clone().lock_owned().await
    }
    // The lifecycle service holds the writer guard across inspection, Docker removal and deletion.
    pub(crate) async fn secret_deletion_versions(
        &self,
        application: &ApplicationId,
        name: &str,
        expected: i64,
    ) -> Result<Vec<String>, StoreError> {
        let app = self.get(application).await?;
        let id = application.as_str();
        let generation = sqlx::query_scalar!(
            "SELECT generation FROM application_secrets WHERE application_id=?1 AND name=?2",
            id,
            name
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        Self::secret_version_matches(expected, generation)?;
        let in_saved = app
            .application
            .spec
            .services
            .iter()
            .any(|s| s.secrets.iter().any(|s| s.name == name));
        let pins=sqlx::query_scalar!("SELECT COUNT(*) FROM deployment_secret_pins p JOIN operations o ON o.id=p.operation_id WHERE p.application_id=?1 AND p.name=?2 AND o.state!='superseded' AND o.id=(SELECT id FROM operations WHERE application_id=?1 ORDER BY created_at_ms DESC,id DESC LIMIT 1)",id,name).fetch_one(&self.pool).await.map_err(StoreError::database)?;
        let versions = sqlx::query_scalar!(
            "SELECT swarm_name FROM secret_versions WHERE application_id=?1 AND name=?2",
            id,
            name
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?;
        let active = app
            .resolved
            .as_ref()
            .is_some_and(|r| r.secret_names.values().any(|n| versions.contains(n)));
        if in_saved || pins > 0 || active {
            return Err(StoreError::SecretReferenced);
        }
        Ok(versions)
    }
    pub(crate) async fn delete_secret_rows(
        &self,
        application: &ApplicationId,
        name: &str,
    ) -> Result<(), StoreError> {
        let id = application.as_str();
        let mut tx = self.pool.begin().await.map_err(StoreError::database)?;
        // Superseded deployments cannot be retried; remove only their obsolete pins.
        sqlx::query!(
            "DELETE FROM deployment_secret_pins WHERE application_id=?1 AND name=?2",
            id,
            name
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        sqlx::query!(
            "DELETE FROM application_secrets WHERE application_id=?1 AND name=?2",
            id,
            name
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn rotation_preserves_retry_pins_and_secret_values_never_enter_snapshots() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let store = Store::open(&path).await.unwrap();
        let mut app = piqueld_core::parse_toml(include_str!(
            "../../../../crates/piqueld-core/tests/fixtures/manifests/prebuilt.toml"
        ))
        .unwrap()
        .normalize(ApplicationId::parse("app-secret-test").unwrap());
        let op = store.save_application(&app, None, None).await.unwrap();
        store
            .put_secret(&app.id, "token", 0, b"first-value".to_vec())
            .await
            .unwrap();
        app.spec.services[0]
            .secrets
            .push(piqueld_core::manifest::SecretMount {
                name: "token".into(),
                target: "/run/secrets/token".into(),
            });
        let pins = store.pin_secrets(&op.id, &app).await.unwrap();
        store
            .put_secret(&app.id, "token", 1, b"second-value".to_vec())
            .await
            .unwrap();
        assert!(
            store
                .put_secret(&app.id, "token", 1, b"stale".to_vec())
                .await
                .is_err()
        );
        assert_eq!(
            store
                .secret_plaintext(&app.id, &pins["token"])
                .await
                .unwrap()
                .as_slice(),
            b"first-value"
        );
        assert!(matches!(
            store.secret_deletion_versions(&app.id, "token", 2).await,
            Err(StoreError::SecretReferenced)
        ));
        assert!(
            store
                .secret_plaintext(
                    &ApplicationId::parse("app-another").unwrap(),
                    &pins["token"]
                )
                .await
                .is_err()
        );
        drop(store);
        let store = Store::open(&path).await.unwrap();
        assert_eq!(
            store.pin_secrets(&op.id, &app).await.unwrap(),
            pins,
            "retry must use the pre-rotation pin after restart"
        );
        assert!(
            !serde_json::to_string(&store.deployment_manifest(&op.id).await.unwrap())
                .unwrap()
                .contains("first-value")
        );
        let next = store.request_deploy(&app.id, None).await.unwrap();
        let new_pins = store.pin_secrets(&next.id, &app).await.unwrap();
        assert_ne!(pins, new_pins);
        assert_eq!(
            store
                .secret_plaintext(&app.id, &new_pins["token"])
                .await
                .unwrap()
                .as_slice(),
            b"second-value"
        );
        std::fs::remove_file(directory.path().join("secrets.key")).unwrap();
        assert_eq!(
            store.secrets(&app.id).await.unwrap()[0].generation,
            2,
            "metadata reads do not require the key"
        );
        assert!(
            store
                .put_secret(&app.id, "token", 2, b"replacement".to_vec())
                .await
                .is_err()
        );
        assert!(!directory.path().join("secrets.key").exists());
    }
}
