//! Authenticate the database's key before accepting any new ciphertext.
use super::{Envelope, SecretCipher, Store, StoreError};
use piqueld_core::api::SecretKeyRecovery;

impl Store {
    /// Loads the master key and proves it matches the database by decrypting the
    /// stored verifier. The first write binds the key by storing a new verifier
    /// in `tx`, so it only persists if the caller commits.
    pub(super) async fn verified_secret_cipher(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    ) -> Result<SecretCipher, StoreError> {
        let verification =
            sqlx::query!("SELECT nonce,ciphertext FROM secret_key_verification WHERE singleton=1")
                .fetch_optional(&mut **tx)
                .await
                .map_err(StoreError::database)?;
        // Without a verifier no stored value needs a key: the database is new, or
        // lost-key recovery discarded every value. The next write binds a new key.
        let cipher = SecretCipher::load(&self.secret_key_path, verification.is_some())
            .map_err(StoreError::SecretSource)?;
        if let Some(row) = verification {
            cipher
                .decrypt(
                    "piqueld",
                    "key-verification",
                    1,
                    &Envelope {
                        nonce: row.nonce,
                        ciphertext: row.ciphertext,
                    },
                )
                .map_err(StoreError::SecretSource)?;
        } else {
            let envelope = cipher
                .encrypt("piqueld", "key-verification", 1, b"piqueld-secret-key-v1")
                .map_err(StoreError::SecretSource)?;
            sqlx::query!(
                "INSERT INTO secret_key_verification(singleton,nonce,ciphertext) VALUES(1,?1,?2)",
                envelope.nonce,
                envelope.ciphertext
            )
            .execute(&mut **tx)
            .await
            .map_err(StoreError::database)?;
        }
        Ok(cipher)
    }

    /// Recovers from a lost or unusable master key by discarding every stored value,
    /// then moving the old key aside; the next value write generates a new key.
    /// Refuses while the current key works. Repeating it after a crash is safe.
    /// `actor` needs `system:operate`, checked in the recovering transaction.
    /// # Errors
    /// Returns [`StoreError::SecretKeyUsable`] for a working key, or persistence errors.
    pub async fn recover_secret_key(
        &self,
        actor: crate::store::Actor<'_>,
    ) -> Result<SecretKeyRecovery, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let operate = piqueld_core::access::Permission::Global(
            piqueld_core::access::GlobalPermission::SystemOperate,
        );
        actor.require_on(&mut tx, operate, None).await?;
        match self.verified_secret_cipher(&mut tx).await {
            Ok(_) => return Err(StoreError::SecretKeyUsable),
            Err(StoreError::SecretSource(_)) => {}
            Err(error) => return Err(error),
        }
        let usage = sqlx::query!("SELECT COUNT(*) AS versions, COUNT(DISTINCT environment_id) AS applications, COUNT(DISTINCT environment_id||char(0)||name) AS secrets FROM secret_versions WHERE available=1")
            .fetch_one(&mut *tx).await.map_err(StoreError::database)?;
        let now = super::now_ms();
        // Each affected application's history explains why its values need replacing.
        sqlx::query!("INSERT INTO events(application_id,environment_id,kind,message,created_at_ms) SELECT (SELECT application_id FROM environments WHERE id=secret_versions.environment_id),environment_id,'secret_values_discarded','Secret key recovery discarded '||COUNT(*)||' stored values; store replacements, then deploy',?1 FROM secret_versions WHERE available=1 GROUP BY environment_id",now)
            .execute(&mut *tx).await.map_err(StoreError::database)?;
        sqlx::query!(
            "UPDATE secret_versions SET available=0,nonce=X'',ciphertext=X'' WHERE available=1"
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        sqlx::query!("DELETE FROM secret_key_verification")
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        let message = format!(
            "Recovered the secret master key; discarded {} values across {} applications",
            usage.versions, usage.applications
        );
        sqlx::query!("INSERT INTO events(scope,kind,message,created_at_ms) VALUES('daemon','secret_key_recovered',?1,?2)",message,now)
            .execute(&mut *tx).await.map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)?;
        SecretCipher::retire(&self.secret_key_path, now).map_err(StoreError::SecretSource)?;
        Ok(SecretKeyRecovery {
            affected_environments: usage.applications,
            affected_secrets: usage.secrets,
            discarded_versions: usage.versions,
        })
    }
}
