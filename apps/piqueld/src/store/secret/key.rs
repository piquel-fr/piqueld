//! Authenticate the database's key before accepting any new ciphertext.
use super::{Envelope, SecretCipher, Store, StoreError};
use futures_util::TryStreamExt;
use piqueld_core::api::SecretKeyReplacement;

impl Store {
    pub(super) async fn verified_secret_cipher(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    ) -> Result<SecretCipher, StoreError> {
        self.finish_key_replacement_on(tx).await?;
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
                "SELECT application_id,name,generation,nonce,ciphertext FROM secret_versions WHERE available=1"
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

impl Store {
    /// Finish an interrupted commit on startup. Missing keys must still allow
    /// metadata access and explicit destructive recovery through the API.
    pub(crate) async fn recover_secret_key(&self) -> Result<(), StoreError> {
        let (_writer, tx) = self.begin_immediate().await?;
        match self.complete_key_replacement(tx).await {
            Err(error @ StoreError::SecretSource(_)) => {
                tracing::error!(
                    ?error,
                    "secret key recovery incomplete; metadata and explicit recovery remain available"
                );
                Ok(())
            }
            result => result,
        }
    }

    async fn complete_key_replacement(
        &self,
        mut tx: sqlx::Transaction<'_, sqlx::Sqlite>,
    ) -> Result<(), StoreError> {
        self.finish_key_replacement_on(&mut tx).await?;
        tx.commit().await.map_err(StoreError::database)?;
        SecretCipher::cleanup_staged_keys(&self.secret_key_path).map_err(StoreError::SecretSource)
    }

    /// Prevent key recovery from invalidating versions partway through a rollout.
    pub(crate) async fn protect_secret_versions(&self) -> tokio::sync::RwLockReadGuard<'_, ()> {
        self.secret_deployments.read().await
    }

    /// Re-encrypt all values, or explicitly discard them when recovering a lost key.
    /// The durable staged key precedes the SQLite commit; the committed verifier
    /// selects it after interruption. Only then is secrets.key atomically replaced.
    /// # Errors
    /// Returns key/authentication, replay-conflict, or persistence errors.
    pub async fn replace_secret_key(
        &self,
        discard_values: bool,
        request_id: Option<&str>,
    ) -> Result<SecretKeyReplacement, StoreError> {
        let _deployments = self.secret_deployments.write().await;
        let (_writer, mut tx) = self.begin_immediate().await?;
        let fingerprint = format!("replace-secret-key:{discard_values}");
        let now = super::now_ms();
        if let Some(response) = Self::replay_on(&mut tx, request_id, &fingerprint, now).await? {
            self.complete_key_replacement(tx).await?;
            return Ok(response);
        }
        let response = self.stage_key_replacement(&mut tx, discard_values).await?;
        Self::record_receipt_on(&mut tx, request_id, &fingerprint, &response, now).await?;
        tx.commit().await.map_err(StoreError::database)?;
        // Keep the writer lock through file installation. If this future is
        // cancelled, the next value operation finishes the committed replacement.
        let tx = self.pool.begin().await.map_err(StoreError::database)?;
        self.complete_key_replacement(tx).await?;
        Ok(response)
    }

    pub(super) async fn stage_key_replacement(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        discard_values: bool,
    ) -> Result<SecretKeyReplacement, StoreError> {
        let old = if discard_values {
            None
        } else {
            Some(self.verified_secret_cipher(tx).await?)
        };
        let usage = sqlx::query!("SELECT COUNT(*) AS versions, COUNT(DISTINCT application_id) AS applications FROM secret_versions WHERE available=1")
            .fetch_one(&mut **tx).await.map_err(StoreError::database)?;
        let secrets = sqlx::query_scalar!("SELECT COUNT(*) FROM (SELECT DISTINCT application_id,name FROM secret_versions WHERE available=1)")
            .fetch_one(&mut **tx).await.map_err(StoreError::database)?;
        // A unique path never overwrites the key of an earlier committed recovery.
        let id = uuid::Uuid::now_v7();
        let staged = SecretCipher::replacement_path(&self.secret_key_path, id);
        SecretCipher::create(&staged).map_err(StoreError::SecretSource)?;
        let replacement = SecretCipher::load(&staged, true).map_err(StoreError::SecretSource)?;
        if let Some(old) = old {
            Self::reencrypt_secret_values(tx, &old, &replacement).await?;
        } else {
            sqlx::query!(
                "UPDATE secret_versions SET available=0,nonce=X'',ciphertext=X'' WHERE available=1"
            )
            .execute(&mut **tx)
            .await
            .map_err(StoreError::database)?;
        }
        let verifier = replacement
            .encrypt("piqueld", "key-verification", 1, b"piqueld-secret-key-v1")
            .map_err(StoreError::SecretSource)?;
        let pending = id.to_string();
        sqlx::query!("INSERT INTO secret_key_verification(singleton,nonce,ciphertext,pending_key) VALUES(1,?1,?2,?3) ON CONFLICT(singleton) DO UPDATE SET nonce=excluded.nonce,ciphertext=excluded.ciphertext,pending_key=excluded.pending_key",verifier.nonce,verifier.ciphertext,pending)
            .execute(&mut **tx).await.map_err(StoreError::database)?;
        Ok(SecretKeyReplacement {
            discarded_values: discard_values,
            affected_applications: usage.applications,
            affected_secrets: secrets,
            affected_versions: usage.versions,
        })
    }

    async fn reencrypt_secret_values(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        old: &SecretCipher,
        replacement: &SecretCipher,
    ) -> Result<(), StoreError> {
        // Read one bounded value at a time, including historical deployment pins.
        let mut cursor = 0_i64;
        while let Some(row) = sqlx::query!("SELECT rowid AS 'row_id!',application_id,name,generation,nonce,ciphertext FROM secret_versions WHERE available=1 AND rowid>?1 ORDER BY rowid LIMIT 1",cursor)
            .fetch_optional(&mut **tx).await.map_err(StoreError::database)? {
            let plaintext = old.decrypt(&row.application_id, &row.name, row.generation, &Envelope { nonce: row.nonce, ciphertext: row.ciphertext })
                .map_err(StoreError::SecretSource)?;
            let encrypted = replacement.encrypt(&row.application_id, &row.name, row.generation, &plaintext)
                .map_err(StoreError::SecretSource)?;
            sqlx::query!("UPDATE secret_versions SET nonce=?1,ciphertext=?2 WHERE rowid=?3",encrypted.nonce,encrypted.ciphertext,row.row_id)
                .execute(&mut **tx).await.map_err(StoreError::database)?;
            cursor = row.row_id;
        }
        Ok(())
    }

    pub(super) async fn finish_key_replacement_on(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    ) -> Result<(), StoreError> {
        let Some(row) = sqlx::query!("SELECT nonce,ciphertext,pending_key FROM secret_key_verification WHERE pending_key IS NOT NULL")
            .fetch_optional(&mut **tx).await.map_err(StoreError::database)? else {
            return Ok(());
        };
        let id = uuid::Uuid::parse_str(row.pending_key.as_deref().ok_or(StoreError::Corrupt)?)
            .map_err(StoreError::corrupt)?;
        let staged = SecretCipher::replacement_path(&self.secret_key_path, id);
        let exists = staged
            .try_exists()
            .map_err(|e| StoreError::SecretSource(e.into()))?;
        let path = if exists {
            &staged
        } else {
            &self.secret_key_path
        };
        let cipher = SecretCipher::load(path, true).map_err(StoreError::SecretSource)?;
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
        if exists {
            SecretCipher::install(&staged, &self.secret_key_path)
                .map_err(StoreError::SecretSource)?;
        } else {
            // The rename may already have succeeded before interruption.
            SecretCipher::sync_directory(&self.secret_key_path)
                .map_err(StoreError::SecretSource)?;
        }
        sqlx::query!("UPDATE secret_key_verification SET pending_key=NULL WHERE singleton=1")
            .execute(&mut **tx)
            .await
            .map_err(StoreError::database)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
