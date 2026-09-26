use super::*;

fn application() -> NormalizedApplication {
    piqueld_core::parse_toml(include_str!(
        "../../../../../crates/piqueld-core/tests/fixtures/manifests/prebuilt.toml"
    ))
    .unwrap()
    .normalize(ApplicationId::parse("app-secret-test").unwrap())
}

fn with_secret(app: &NormalizedApplication) -> NormalizedApplication {
    let mut manifest = app.to_manifest();
    manifest.spec.services[0]
        .secrets
        .push(piqueld_core::manifest::SecretMount {
            name: "token".into(),
            target: "/run/secrets/token".into(),
        });
    manifest.validate().unwrap().normalize(app.id().clone())
}

#[tokio::test]
async fn captured_deployment_protects_secrets_before_pinning() {
    use crate::api::Mutation;
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    let app = application();
    let captured = with_secret(&app);
    let op = store.save_application(&captured, None, None).await.unwrap();
    store
        .put_secret(app.id(), "token", 0, b"value".to_vec())
        .await
        .unwrap();
    store
        .accept(
            Mutation::Save {
                application: app.clone(),
                expected_application_id: Some(app.id().to_string()),
                deploy: false,
            },
            Some(1),
            false,
            None,
        )
        .await
        .unwrap();
    assert!(matches!(
        store.begin_secret_deletion(app.id(), "token", 1).await,
        Err(StoreError::SecretReferenced)
    ));
    assert_eq!(store.pin_secrets(&op.id, &captured).await.unwrap().len(), 1);
}

#[tokio::test]
async fn deletion_reservations_survive_restart_and_do_not_block_other_writes() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let store = Store::open(&path).await.unwrap();
    let app = application();
    let op = store.save_application(&app, None, None).await.unwrap();
    store
        .put_secret(app.id(), "token", 0, b"value".to_vec())
        .await
        .unwrap();
    let deletion = store
        .begin_secret_deletion(app.id(), "token", 1)
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        store.put_secret(app.id(), "other", 0, b"unrelated".to_vec()),
    )
    .await
    .unwrap()
    .unwrap();
    drop(store);
    let store = Store::open(&path).await.unwrap();
    assert!(
        store
            .secrets(app.id())
            .await
            .unwrap()
            .iter()
            .find(|s| s.name == "token")
            .unwrap()
            .deleting
    );
    assert!(matches!(
        store
            .put_secret(app.id(), "token", 1, b"replacement".to_vec())
            .await,
        Err(StoreError::SecretDeleting)
    ));
    assert!(matches!(
        store.save_application(&with_secret(&app), None, None).await,
        Err(StoreError::SecretDeleting)
    ));
    assert!(matches!(
        store.pin_secrets(&op.id, &with_secret(&app)).await,
        Err(StoreError::SecretDeleting)
    ));
    let retry = store
        .begin_secret_deletion(app.id(), "token", 1)
        .await
        .unwrap();
    assert_eq!(retry.id, deletion.id);
    assert_eq!(retry.versions, deletion.versions);
    store
        .finish_secret_deletion(app.id(), "token", &retry.id)
        .await
        .unwrap();
    store
        .put_secret(app.id(), "token", 0, b"new value".to_vec())
        .await
        .unwrap();
    store
        .finish_secret_deletion(app.id(), "token", &deletion.id)
        .await
        .unwrap();
    let names = store.secret_names(app.id()).await.unwrap();
    assert_eq!(
        names.len(),
        2,
        "late cleanup must preserve the recreated secret"
    );
    assert!(!names.contains(&deletion.versions[0]));
}

#[tokio::test]
async fn wrong_master_key_blocks_writes_and_original_key_restores_them() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let key = temp.path().join("secrets.key");
    let store = Store::open(&path).await.unwrap();
    let app = application();
    store.save_application(&app, None, None).await.unwrap();
    store
        .put_secret(app.id(), "token", 0, b"original".to_vec())
        .await
        .unwrap();
    let original = Zeroizing::new(std::fs::read(&key).unwrap());
    let versions = store.secret_names(app.id()).await.unwrap();
    drop(store);
    std::fs::write(&key, [42; 32]).unwrap();
    let store = Store::open(&path).await.unwrap();
    assert!(matches!(
        store
            .put_secret(app.id(), "token", 1, b"wrong key".to_vec())
            .await,
        Err(StoreError::SecretSource(_))
    ));
    assert_eq!(store.secrets(app.id()).await.unwrap()[0].generation, 1);
    std::fs::write(&key, &*original).unwrap();
    store
        .put_secret(app.id(), "token", 1, b"restored".to_vec())
        .await
        .unwrap();
    assert_eq!(
        &*store
            .secret_plaintext(app.id(), &versions[0])
            .await
            .unwrap(),
        b"original"
    );
    let deletion = store
        .begin_secret_deletion(app.id(), "token", 2)
        .await
        .unwrap();
    store
        .finish_secret_deletion(app.id(), "token", &deletion.id)
        .await
        .unwrap();
    std::fs::remove_file(&key).unwrap();
    assert!(matches!(
        store
            .put_secret(app.id(), "new", 0, b"value".to_vec())
            .await,
        Err(StoreError::SecretSource(_))
    ));
    assert!(
        !key.exists(),
        "the database key binding survives deletion of all values"
    );
}

#[tokio::test]
async fn upgrade_authenticates_all_retained_ciphertext_before_binding_key() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
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
    sqlx::query!("DELETE FROM secret_key_verification")
        .execute(&store.pool)
        .await
        .unwrap();
    let original = sqlx::query_scalar!("SELECT ciphertext FROM secret_versions WHERE generation=2")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    sqlx::query!(
        "UPDATE secret_versions SET ciphertext=zeroblob(length(ciphertext)) WHERE generation=2"
    )
    .execute(&store.pool)
    .await
    .unwrap();
    assert!(matches!(
        store
            .put_secret(app.id(), "token", 2, b"third".to_vec())
            .await,
        Err(StoreError::SecretSource(_))
    ));
    assert_eq!(
        sqlx::query_scalar!("SELECT COUNT(*) FROM secret_key_verification")
            .fetch_one(&store.pool)
            .await
            .unwrap(),
        0
    );
    sqlx::query!(
        "UPDATE secret_versions SET ciphertext=?1 WHERE generation=2",
        original
    )
    .execute(&store.pool)
    .await
    .unwrap();
    store
        .put_secret(app.id(), "token", 2, b"third".to_vec())
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar!("SELECT COUNT(*) FROM secret_key_verification")
            .fetch_one(&store.pool)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn retained_version_quota_rejects_writes_without_removing_values() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    let app = application();
    store.save_application(&app, None, None).await.unwrap();
    store
        .put_secret(app.id(), "token", 0, b"x".to_vec())
        .await
        .unwrap();
    let mut tx = store.pool.begin().await.unwrap();
    // Existing ciphertext is 17 bytes; the incoming value adds its own 16-byte tag.
    Store::check_secret_quota(&mut tx, app.id().as_str(), 100 * 1024 * 1024 - 33)
        .await
        .unwrap();
    assert!(matches!(
        Store::check_secret_quota(&mut tx, app.id().as_str(), 100 * 1024 * 1024 - 32).await,
        Err(StoreError::SecretQuota)
    ));
    tx.rollback().await.unwrap();
    sqlx::query!("WITH RECURSIVE versions(n) AS (SELECT 2 UNION ALL SELECT n+1 FROM versions WHERE n<1000) INSERT INTO secret_versions(application_id,name,generation,swarm_name,nonce,ciphertext) SELECT s.application_id,s.name,v.n,'fixture-'||v.n,s.nonce,s.ciphertext FROM versions v CROSS JOIN secret_versions s WHERE s.generation=1").execute(&store.pool).await.unwrap();
    assert!(matches!(
        store
            .put_secret(app.id(), "token", 1, b"blocked".to_vec())
            .await,
        Err(StoreError::SecretQuota)
    ));
    assert_eq!(store.secrets(app.id()).await.unwrap()[0].generation, 1);
    assert_eq!(store.secret_names(app.id()).await.unwrap().len(), 1000);
    let deletion = store
        .begin_secret_deletion(app.id(), "token", 1)
        .await
        .unwrap();
    store
        .finish_secret_deletion(app.id(), "token", &deletion.id)
        .await
        .unwrap();
    store
        .put_secret(app.id(), "fresh", 0, b"space freed".to_vec())
        .await
        .unwrap();
}
#[tokio::test]
async fn rotation_preserves_retry_pins_and_secret_values_never_enter_snapshots() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let store = Store::open(&path).await.unwrap();
    let mut app = application();
    let op = store.save_application(&app, None, None).await.unwrap();
    store
        .put_secret(app.id(), "token", 0, b"first-value".to_vec())
        .await
        .unwrap();
    let mut input = app.to_manifest();
    input.spec.services[0]
        .secrets
        .push(piqueld_core::manifest::SecretMount {
            name: "token".into(),
            target: "/run/secrets/token".into(),
        });
    app = input.validate().unwrap().normalize(app.id().clone());
    let pins = store.pin_secrets(&op.id, &app).await.unwrap();
    store
        .put_secret(app.id(), "token", 1, b"second-value".to_vec())
        .await
        .unwrap();
    assert!(
        store
            .put_secret(app.id(), "token", 1, b"stale".to_vec())
            .await
            .is_err()
    );
    assert_eq!(
        store
            .secret_plaintext(app.id(), &pins["token"])
            .await
            .unwrap()
            .as_slice(),
        b"first-value"
    );
    assert!(matches!(
        store.begin_secret_deletion(app.id(), "token", 2).await,
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
    let next = store.request_deploy(app.id(), None).await.unwrap();
    let new_pins = store.pin_secrets(&next.id, &app).await.unwrap();
    assert_ne!(pins, new_pins);
    assert_eq!(
        store
            .secret_plaintext(app.id(), &new_pins["token"])
            .await
            .unwrap()
            .as_slice(),
        b"second-value"
    );
    std::fs::remove_file(directory.path().join("secrets.key")).unwrap();
    assert_eq!(
        store.secrets(app.id()).await.unwrap()[0].generation,
        2,
        "metadata reads do not require the key"
    );
    assert!(
        store
            .put_secret(app.id(), "token", 2, b"replacement".to_vec())
            .await
            .is_err()
    );
    assert!(!directory.path().join("secrets.key").exists());
}
