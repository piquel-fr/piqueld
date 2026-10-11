//! Authenticate the database's key before accepting any new ciphertext.
use super::{Envelope, SecretCipher, SecretOwner, Store, StoreError};
use crate::secrets::KeyFailure;
use piqueld_core::api::{CheckStatus, SecretKeyRecovery, SystemCheck, SystemCheckId};

impl SecretCipher {
    /// Proves this key matches the database by decrypting its stored verifier.
    fn verify(&self, verifier: &Envelope) -> anyhow::Result<()> {
        self.decrypt(SecretOwner::KeyVerifier, "key-verification", 1, verifier)
            .map(drop)
    }
}

impl Store {
    /// Checks without side effects that `secrets.key` exists while values
    /// need it, is private, and matches the stored verifier. Unlike
    /// [`Self::verified_secret_cipher`], it never generates a key or stores a
    /// verifier.
    pub(crate) async fn secret_key_check(&self) -> SystemCheck {
        let check =
            |status, message: String| SystemCheck::new(SystemCheckId::SecretKey, status, message);
        let path = self.secret_key_path.display();
        let verifier = match sqlx::query!(
            "SELECT nonce,ciphertext FROM secret_key_verification WHERE singleton=1"
        )
        .fetch_optional(&self.pool)
        .await
        {
            Ok(row) => row.map(|row| Envelope {
                nonce: row.nonce,
                ciphertext: row.ciphertext,
            }),
            Err(error) => {
                return check(
                    CheckStatus::Fail,
                    format!("could not read the stored key verifier: {error}"),
                )
                .next_step("Check the database; daemon logs have details.");
            }
        };
        match (self.secret_key_path.try_exists(), &verifier) {
            (Ok(false), None) => {
                return check(
                    CheckStatus::Pass,
                    format!("no secret is stored yet; {path} is created by the first secret write"),
                );
            }
            (Ok(false), Some(_)) => {
                return check(CheckStatus::Fail, KeyFailure::Missing.to_string()).next_step(
                    format!("Restore {path} from backup, or discard every stored value with `piquelctl secrets recover-key`."),
                );
            }
            (Ok(true), _) => {}
            (Err(error), _) => {
                return check(
                    CheckStatus::Fail,
                    format!("could not inspect {path}: {error}"),
                )
                .next_step("Check the data directory's permissions.");
            }
        }
        let opened = SecretCipher::open(&self.secret_key_path).and_then(|cipher| match &verifier {
            Some(verifier) => cipher
                .verify(verifier)
                .map_err(|_| KeyFailure::AuthenticationFailed.into()),
            None => Ok(()),
        });
        match opened {
            Ok(()) if verifier.is_some() => check(
                CheckStatus::Pass,
                format!("{path} is private and matches the stored verifier"),
            ),
            Ok(()) => check(
                CheckStatus::Pass,
                format!("{path} is private; the first secret write binds it to the database"),
            ),
            Err(error) => {
                let step = match error.downcast_ref::<KeyFailure>() {
                    Some(KeyFailure::NotPrivate) => format!(
                        "Make {path} a regular file owned by the daemon user with mode 0600."
                    ),
                    _ => format!(
                        "Restore the original {path} from backup, or discard every stored value with `piquelctl secrets recover-key`."
                    ),
                };
                check(CheckStatus::Fail, format!("{error:#}")).next_step(step)
            }
        }
    }

    /// Loads the master key and proves it matches the database by decrypting the
    /// stored verifier. The first write binds the key by storing a new verifier
    /// in `tx`, so it only persists if the caller commits.
    pub(in crate::store) async fn verified_secret_cipher(
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
                .verify(&Envelope {
                    nonce: row.nonce,
                    ciphertext: row.ciphertext,
                })
                .map_err(StoreError::SecretSource)?;
        } else {
            let envelope = cipher
                .encrypt(
                    SecretOwner::KeyVerifier,
                    "key-verification",
                    1,
                    b"piqueld-secret-key-v1",
                )
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
        let usage = sqlx::query!("SELECT COUNT(*) AS versions, COUNT(DISTINCT environment_id) AS environments, COUNT(DISTINCT environment_id||char(0)||name) AS secrets FROM secret_versions WHERE available=1")
            .fetch_one(&mut *tx).await.map_err(StoreError::database)?;
        let stored = sqlx::query!("SELECT COUNT(*) AS versions, COUNT(DISTINCT application_id) AS applications, COUNT(DISTINCT application_id||char(0)||name) AS secrets FROM application_secret_versions WHERE available=1")
            .fetch_one(&mut *tx).await.map_err(StoreError::database)?;
        let now = super::now_ms();
        let by = actor.attribution();
        let operator = by.operator_uid();
        // Each affected environment's and application's history explains why
        // its values need replacing.
        sqlx::query!("INSERT INTO events(application_id,environment_id,kind,message,created_at_ms,actor_user_id,actor_credential_id,actor_operator_uid) SELECT (SELECT application_id FROM environments WHERE id=secret_versions.environment_id),environment_id,'secret_values_discarded','Secret key recovery discarded '||COUNT(*)||' stored values; store replacements, then deploy',?1,?2,?3,?4 FROM secret_versions WHERE available=1 GROUP BY environment_id",now,by.user_id,by.credential_id,operator)
            .execute(&mut *tx).await.map_err(StoreError::database)?;
        sqlx::query!("INSERT INTO events(application_id,kind,message,created_at_ms,actor_user_id,actor_credential_id,actor_operator_uid) SELECT application_id,'secret_values_discarded','Secret key recovery discarded '||COUNT(*)||' stored values; store replacements, then deploy',?1,?2,?3,?4 FROM application_secret_versions WHERE available=1 GROUP BY application_id",now,by.user_id,by.credential_id,operator)
            .execute(&mut *tx).await.map_err(StoreError::database)?;
        sqlx::query!(
            "UPDATE secret_versions SET available=0,nonce=X'',ciphertext=X'' WHERE available=1"
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        sqlx::query!(
            "UPDATE application_secret_versions SET available=0,nonce=X'',ciphertext=X'',moved_from=NULL WHERE available=1"
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        // Webhook secrets are encrypted too; applications generate new ones.
        sqlx::query!("DELETE FROM webhook_secrets")
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        sqlx::query!("DELETE FROM secret_key_verification")
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        let discarded = usage.versions + stored.versions;
        let message = format!(
            "Recovered the secret master key; discarded {discarded} values across {} environments and {} applications",
            usage.environments, stored.applications
        );
        let recovered = crate::store::SecurityEvent::SecretKeyRecovered;
        Self::security_event_on(&mut tx, recovered, &message, by).await?;
        tx.commit().await.map_err(StoreError::database)?;
        SecretCipher::retire(&self.secret_key_path, now).map_err(StoreError::SecretSource)?;
        Ok(SecretKeyRecovery {
            affected_environments: usage.environments,
            affected_applications: stored.applications,
            affected_secrets: usage.secrets + stored.secrets,
            discarded_versions: discarded,
        })
    }
}
