use super::*;
use crate::store::secret::tests::{application, with_secret};
use piqueld_core::{ApplicationId, ApplicationName};
use zeroize::Zeroizing;

struct Fixture {
    directory: tempfile::TempDir,
    store: Store,
    app: piqueld_core::NormalizedApplication,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("db")).await.unwrap();
        let app = application();
        store.save_application(&app, None, None).await.unwrap();
        store
            .put_secret(app.id(), "token", 0, b"first".to_vec())
            .await
            .unwrap();
        store
            .put_secret(app.id(), "token", 1, b"second".to_vec())
            .await
            .unwrap();
        Self {
            directory,
            store,
            app,
        }
    }

    fn key(&self) -> Zeroizing<Vec<u8>> {
        Zeroizing::new(std::fs::read(self.directory.path().join("secrets.key")).unwrap())
    }

    async fn values(&self) -> Vec<Vec<u8>> {
        let mut result = Vec::new();
        for name in self.store.secret_names(self.app.id()).await.unwrap() {
            result.push(
                self.store
                    .secret_plaintext(self.app.id(), &name)
                    .await
                    .unwrap()
                    .to_vec(),
            );
        }
        result.sort();
        result
    }

    async fn reopen(&mut self) {
        self.store.pool.close().await;
        self.store = Store::open(self.directory.path().join("db")).await.unwrap();
    }
}

#[tokio::test]
async fn secret_key_rotation_preserves_every_application_version_and_pin() {
    let mut f = Fixture::new().await;
    let other = f
        .app
        .clone()
        .with_id(ApplicationId::parse("app-other").unwrap())
        .with_name(ApplicationName::parse("other").unwrap());
    f.store.save_application(&other, None, None).await.unwrap();
    f.store
        .put_secret(other.id(), "other", 0, b"third".to_vec())
        .await
        .unwrap();
    let op = f.store.request_deploy(f.app.id(), None).await.unwrap();
    let pins = f
        .store
        .pin_secrets(&op.id, &with_secret(&f.app))
        .await
        .unwrap();
    let original_key = f.key();
    let before = f.values().await;
    let outcome = f
        .store
        .replace_secret_key(false, Some("rotation"))
        .await
        .unwrap();
    assert_eq!(
        (
            outcome.affected_applications,
            outcome.affected_secrets,
            outcome.affected_versions
        ),
        (2, 2, 3)
    );
    assert!(!outcome.discarded_values);
    assert_ne!(*original_key, *f.key());
    assert_eq!(f.values().await, before);
    assert_eq!(
        f.store
            .pin_secrets(&op.id, &with_secret(&f.app))
            .await
            .unwrap(),
        pins
    );
    assert_eq!(f.store.secrets(f.app.id()).await.unwrap()[0].generation, 2);
    let replacement_key = f.key();
    assert_eq!(
        f.store
            .replace_secret_key(false, Some("rotation"))
            .await
            .unwrap(),
        outcome
    );
    assert_eq!(*replacement_key, *f.key(), "a replay must not rotate again");
    f.reopen().await;
    assert_eq!(f.values().await, before);
    std::fs::write(&f.store.secret_key_path, &*original_key).unwrap();
    assert!(matches!(
        f.store
            .put_secret(f.app.id(), "token", 2, b"wrong-key".to_vec())
            .await,
        Err(StoreError::SecretSource(_))
    ));
}

#[tokio::test]
async fn secret_key_recovery_preserves_names_but_never_rebinds_old_pins() {
    let mut f = Fixture::new().await;
    let op = f.store.request_deploy(f.app.id(), None).await.unwrap();
    let manifest = with_secret(&f.app);
    let pins = f.store.pin_secrets(&op.id, &manifest).await.unwrap();
    std::fs::remove_file(&f.store.secret_key_path).unwrap();
    assert!(matches!(
        f.store.replace_secret_key(false, None).await,
        Err(StoreError::SecretSource(_))
    ));
    assert!(!f.store.secret_key_path.exists());
    let recovery = f
        .store
        .replace_secret_key(true, Some("recovery"))
        .await
        .unwrap();
    assert!(recovery.discarded_values);
    assert_eq!(recovery.affected_versions, 2);
    let metadata = f.store.secrets(f.app.id()).await.unwrap().remove(0);
    assert_eq!(metadata.generation, 2);
    assert!(metadata.unavailable);
    let nonempty = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM secret_versions WHERE length(ciphertext)!=0 OR length(nonce)!=0"
    )
    .fetch_one(&f.store.pool)
    .await
    .unwrap();
    assert_eq!(nonempty, 0);
    assert!(matches!(
        f.store.secret_plaintext(f.app.id(), &pins["token"]).await,
        Err(StoreError::SecretUnavailable { .. })
    ));
    let next = f.store.request_deploy(f.app.id(), None).await.unwrap();
    assert!(matches!(
        f.store.pin_secrets(&next.id, &manifest).await,
        Err(StoreError::SecretUnavailable { .. })
    ));
    let updated = f
        .store
        .put_secret(f.app.id(), "token", 2, b"replacement".to_vec())
        .await
        .unwrap();
    assert_eq!(updated.generation, 3);
    assert!(!updated.unavailable);
    assert!(matches!(
        f.store.pin_secrets(&op.id, &manifest).await,
        Err(StoreError::SecretUnavailable { .. })
    ));
    let new_pins = f.store.pin_secrets(&next.id, &manifest).await.unwrap();
    assert_ne!(pins, new_pins);
    // An uncertain HTTP outcome may be retried after replacement values were entered.
    assert_eq!(
        f.store
            .replace_secret_key(true, Some("recovery"))
            .await
            .unwrap(),
        recovery
    );
    assert!(matches!(
        f.store.replace_secret_key(false, Some("recovery")).await,
        Err(StoreError::ReplayConflict)
    ));
    f.store.replace_secret_key(false, None).await.unwrap();
    f.reopen().await;
    assert_eq!(
        &*f.store
            .secret_plaintext(f.app.id(), &new_pins["token"])
            .await
            .unwrap(),
        b"replacement"
    );
    assert!(matches!(
        f.store.secret_plaintext(f.app.id(), &pins["token"]).await,
        Err(StoreError::SecretUnavailable { .. })
    ));
}

#[tokio::test]
async fn secret_key_replacement_recovers_at_each_commit_boundary() {
    for discard_values in [false, true] {
        for committed in [false, true] {
            for installed in [false, true] {
                if installed && !committed {
                    continue;
                }
                let mut f = Fixture::new().await;
                let original_key = f.key();
                let before = f.values().await;
                let mut tx = f.store.pool.begin().await.unwrap();
                f.store
                    .stage_key_replacement(&mut tx, discard_values)
                    .await
                    .unwrap();
                if committed {
                    tx.commit().await.unwrap();
                    if installed {
                        let mut tx = f.store.pool.begin().await.unwrap();
                        f.store.finish_key_replacement_on(&mut tx).await.unwrap();
                        // Model interruption after rename but before clearing the journal.
                        tx.rollback().await.unwrap();
                    }
                } else {
                    tx.rollback().await.unwrap();
                }
                f.reopen().await;
                if committed && discard_values {
                    assert!(f.store.secrets(f.app.id()).await.unwrap()[0].unavailable);
                } else {
                    assert_eq!(f.values().await, before);
                }
                assert!(
                    !std::fs::read_dir(f.directory.path())
                        .unwrap()
                        .any(|entry| entry
                            .unwrap()
                            .file_name()
                            .to_string_lossy()
                            .ends_with(".pending"))
                );
                assert_eq!(*original_key == *f.key(), !committed);
                let pending = sqlx::query_scalar!(
                    "SELECT pending_key FROM secret_key_verification WHERE singleton=1"
                )
                .fetch_one(&f.store.pool)
                .await
                .unwrap();
                assert!(pending.is_none());
                f.store
                    .put_secret(f.app.id(), "token", 2, b"after-restart".to_vec())
                    .await
                    .unwrap();
            }
        }
    }
}

#[tokio::test]
async fn secret_key_rotation_fails_atomically_on_corrupt_historical_ciphertext() {
    let f = Fixture::new().await;
    let original = f.key();
    sqlx::query!(
        "UPDATE secret_versions SET ciphertext=zeroblob(length(ciphertext)) WHERE generation=2"
    )
    .execute(&f.store.pool)
    .await
    .unwrap();
    assert!(matches!(
        f.store.replace_secret_key(false, None).await,
        Err(StoreError::SecretSource(_))
    ));
    assert_eq!(*original, *f.key());
    let first = sqlx::query_scalar!("SELECT swarm_name FROM secret_versions WHERE generation=1")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(
        &*f.store.secret_plaintext(f.app.id(), &first).await.unwrap(),
        b"first"
    );
    assert!(!f.store.secrets(f.app.id()).await.unwrap()[0].unavailable);
}

#[tokio::test]
async fn secret_key_recovery_can_recover_a_lost_pending_key_without_guessing() {
    let mut f = Fixture::new().await;
    let mut tx = f.store.pool.begin().await.unwrap();
    f.store.stage_key_replacement(&mut tx, false).await.unwrap();
    tx.commit().await.unwrap();
    let pending =
        sqlx::query_scalar!("SELECT pending_key FROM secret_key_verification WHERE singleton=1")
            .fetch_one(&f.store.pool)
            .await
            .unwrap()
            .unwrap();
    let staged = SecretCipher::replacement_path(
        &f.store.secret_key_path,
        uuid::Uuid::parse_str(&pending).unwrap(),
    );
    std::fs::write(&staged, [42; 32]).unwrap();
    f.reopen().await;
    assert!(matches!(
        f.store.replace_secret_key(false, None).await,
        Err(StoreError::SecretSource(_))
    ));
    assert_eq!(f.store.secrets(f.app.id()).await.unwrap()[0].generation, 2);
    f.store.replace_secret_key(true, None).await.unwrap();
    f.store
        .put_secret(f.app.id(), "token", 2, b"restored".to_vec())
        .await
        .unwrap();
    f.reopen().await;
    assert!(!f.store.secrets(f.app.id()).await.unwrap()[0].unavailable);
}

#[tokio::test]
async fn secret_key_recovery_waits_for_rollouts_without_holding_the_database_writer() {
    let f = Fixture::new().await;
    let guard = f.store.protect_secret_versions().await;
    let recovery = f.store.replace_secret_key(true, None);
    tokio::pin!(recovery);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), &mut recovery)
            .await
            .is_err()
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        f.store
            .put_secret(f.app.id(), "token", 2, b"concurrent".to_vec()),
    )
    .await
    .unwrap()
    .unwrap();
    drop(guard);
    assert_eq!(recovery.await.unwrap().affected_versions, 3);
}
