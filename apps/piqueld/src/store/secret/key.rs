//! Authenticate the database's key before accepting any new ciphertext.
use super::{Envelope, SecretCipher, Store, StoreError};
use futures_util::TryStreamExt;

impl Store {
    pub(super) async fn verified_secret_cipher(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    ) -> Result<SecretCipher, StoreError> {
        let verification =
            sqlx::query!("SELECT nonce,ciphertext FROM secret_key_verification WHERE singleton=1")
                .fetch_optional(&mut **tx)
                .await
                .map_err(StoreError::database)?;
        let exists = sqlx::query_scalar!("SELECT COUNT(*) FROM secret_versions")
            .fetch_one(&mut **tx)
            .await
            .map_err(StoreError::database)?;
        let cipher =
            SecretCipher::load(&self.secret_key_path, exists > 0 || verification.is_some())
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
            // On upgrade, authenticate every retained version before binding this key.
            // Stream rows so a large existing database does not load into memory.
            let mut rows = sqlx::query!(
                "SELECT application_id,name,generation,nonce,ciphertext FROM secret_versions"
            )
            .fetch(&mut **tx);
            while let Some(row) = rows.try_next().await.map_err(StoreError::database)? {
                cipher
                    .decrypt(
                        &row.application_id,
                        &row.name,
                        row.generation,
                        &Envelope {
                            nonce: row.nonce,
                            ciphertext: row.ciphertext,
                        },
                    )
                    .map_err(StoreError::SecretSource)?;
            }
            drop(rows);
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
}
