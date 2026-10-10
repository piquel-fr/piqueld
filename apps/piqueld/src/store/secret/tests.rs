use super::*;
use crate::api::Actor::Daemon;
use crate::store::Attribution;
use piqueld_core::manifest::ApplicationTemplate;

fn application() -> NormalizedApplication {
    piqueld_core::parse_toml(include_str!(
        "../../../../../crates/piqueld-core/tests/fixtures/manifests/prebuilt.toml"
    ))
    .unwrap()
    .normalize(piqueld_core::ApplicationId::parse("app-secret-test").unwrap())
}

/// The environment created with `app`, which shares its ID.
fn environment(app: &NormalizedApplication) -> EnvironmentId {
    EnvironmentId::default_for(app.id())
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

/// `with_secret`, with `token` declared as a generated secret.
fn with_generated_secret(app: &NormalizedApplication) -> NormalizedApplication {
    let mut manifest = with_secret(app).to_manifest();
    manifest
        .spec
        .secrets
        .push(piqueld_core::manifest::SecretDeclaration {
            name: "token".into(),
            generate: piqueld_core::manifest::SecretGenerator::Random {
                bytes: 16,
                encoding: piqueld_core::manifest::SecretEncoding::Hex,
            },
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
    let op = store
        .save_application(&ApplicationTemplate::from(&captured), None, None)
        .await
        .unwrap();
    store
        .put_stored_secret(Daemon, app.id(), "token", 0, b"value".to_vec(), None)
        .await
        .unwrap();
    store
        .accept(
            Daemon,
            Mutation::Save {
                application: Box::new(ApplicationTemplate::from(&app)),
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
        store
            .begin_stored_secret_deletion(Daemon, app.id(), "token", 1)
            .await,
        Err(StoreError::SecretReferenced)
    ));
    assert_eq!(store.pin_secrets(&op, &captured).await.unwrap().len(), 1);
}

#[tokio::test]
async fn deletion_reservations_survive_restart_and_do_not_block_other_writes() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let store = Store::open(&path).await.unwrap();
    let app = application();
    let env = environment(&app);
    let op = store
        .save_application(&ApplicationTemplate::from(&app), None, None)
        .await
        .unwrap();
    store
        .put_secret(Daemon, &env, "token", 0, b"value".to_vec())
        .await
        .unwrap();
    let deletion = store
        .begin_secret_deletion(Daemon, &env, "token", 1)
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        store.put_secret(Daemon, &env, "other", 0, b"unrelated".to_vec()),
    )
    .await
    .unwrap()
    .unwrap();
    drop(store);
    let store = Store::open(&path).await.unwrap();
    assert!(
        store
            .secrets(&env)
            .await
            .unwrap()
            .iter()
            .find(|s| s.name == "token")
            .unwrap()
            .deleting
    );
    assert!(matches!(
        store
            .put_secret(Daemon, &env, "token", 1, b"replacement".to_vec())
            .await,
        Err(StoreError::SecretDeleting)
    ));
    assert!(matches!(
        store
            .save_application(&ApplicationTemplate::from(&with_secret(&app)), None, None)
            .await,
        Err(StoreError::SecretDeleting)
    ));
    assert!(matches!(
        store.pin_secrets(&op, &with_secret(&app)).await,
        Err(StoreError::SecretDeleting)
    ));
    let retry = store
        .begin_secret_deletion(Daemon, &env, "token", 1)
        .await
        .unwrap();
    assert_eq!(retry.id, deletion.id);
    assert_eq!(retry.versions, deletion.versions);
    store
        .finish_secret_deletion(Attribution::default(), &env, "token", &retry.id)
        .await
        .unwrap();
    store
        .put_secret(Daemon, &env, "token", 0, b"new value".to_vec())
        .await
        .unwrap();
    store
        .finish_secret_deletion(Attribution::default(), &env, "token", &deletion.id)
        .await
        .unwrap();
    let names = store.secret_names(&env).await.unwrap();
    assert_eq!(
        names.len(),
        2,
        "late cleanup must preserve the recreated secret"
    );
    assert!(!names.contains(&deletion.versions[&environment(&app)][0]));
}

#[tokio::test]
async fn wrong_master_key_blocks_writes_and_original_key_restores_them() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let key = temp.path().join("secrets.key");
    let store = Store::open(&path).await.unwrap();
    let app = application();
    store
        .save_application(&ApplicationTemplate::from(&app), None, None)
        .await
        .unwrap();
    store
        .put_secret(Daemon, &environment(&app), "token", 0, b"original".to_vec())
        .await
        .unwrap();
    let original = Zeroizing::new(std::fs::read(&key).unwrap());
    let versions = store.secret_names(&environment(&app)).await.unwrap();
    drop(store);
    std::fs::write(&key, [42; 32]).unwrap();
    let store = Store::open(&path).await.unwrap();
    assert!(matches!(
        store
            .put_secret(
                Daemon,
                &environment(&app),
                "token",
                1,
                b"wrong key".to_vec()
            )
            .await,
        Err(StoreError::SecretSource(_))
    ));
    assert_eq!(
        store.secrets(&environment(&app)).await.unwrap()[0].generation,
        1
    );
    std::fs::write(&key, &*original).unwrap();
    store
        .put_secret(Daemon, &environment(&app), "token", 1, b"restored".to_vec())
        .await
        .unwrap();
    assert_eq!(
        &*store
            .secret_plaintext(&environment(&app), &versions[0])
            .await
            .unwrap(),
        b"original"
    );
    let deletion = store
        .begin_secret_deletion(Daemon, &environment(&app), "token", 2)
        .await
        .unwrap();
    store
        .finish_secret_deletion(
            Attribution::default(),
            &environment(&app),
            "token",
            &deletion.id,
        )
        .await
        .unwrap();
    std::fs::remove_file(&key).unwrap();
    assert!(matches!(
        store
            .put_secret(Daemon, &environment(&app), "new", 0, b"value".to_vec())
            .await,
        Err(StoreError::SecretSource(_))
    ));
    assert!(
        !key.exists(),
        "the database key binding survives deletion of all values"
    );
}

#[tokio::test]
async fn retained_version_quota_rejects_writes_without_removing_values() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    let app = application();
    store
        .save_application(&ApplicationTemplate::from(&app), None, None)
        .await
        .unwrap();
    store
        .put_secret(Daemon, &environment(&app), "token", 0, b"x".to_vec())
        .await
        .unwrap();
    // Existing ciphertext is 17 bytes; the incoming value adds its own 16-byte tag.
    Store::check_secret_quota(1, 17, 100 * 1024 * 1024 - 33).unwrap();
    assert!(matches!(
        Store::check_secret_quota(1, 17, 100 * 1024 * 1024 - 32),
        Err(StoreError::SecretQuota)
    ));
    sqlx::query!("WITH RECURSIVE versions(n) AS (SELECT 2 UNION ALL SELECT n+1 FROM versions WHERE n<1000) INSERT INTO secret_versions(environment_id,name,generation,swarm_name,nonce,ciphertext) SELECT s.environment_id,s.name,v.n,'fixture-'||v.n,s.nonce,s.ciphertext FROM versions v CROSS JOIN secret_versions s WHERE s.generation=1").execute(&store.pool).await.unwrap();
    assert!(matches!(
        store
            .put_secret(Daemon, &environment(&app), "token", 1, b"blocked".to_vec())
            .await,
        Err(StoreError::SecretQuota)
    ));
    assert_eq!(
        store.secrets(&environment(&app)).await.unwrap()[0].generation,
        1
    );
    assert_eq!(
        store.secret_names(&environment(&app)).await.unwrap().len(),
        1000
    );
    let deletion = store
        .begin_secret_deletion(Daemon, &environment(&app), "token", 1)
        .await
        .unwrap();
    store
        .finish_secret_deletion(
            Attribution::default(),
            &environment(&app),
            "token",
            &deletion.id,
        )
        .await
        .unwrap();
    store
        .put_secret(
            Daemon,
            &environment(&app),
            "fresh",
            0,
            b"space freed".to_vec(),
        )
        .await
        .unwrap();
}
#[tokio::test]
async fn rotation_preserves_retry_pins_and_secret_values_never_enter_snapshots() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let store = Store::open(&path).await.unwrap();
    let mut app = application();
    let op = store
        .save_application(&ApplicationTemplate::from(&app), None, None)
        .await
        .unwrap();
    let env = environment(&app);
    store
        .put_stored_secret(Daemon, app.id(), "token", 0, b"first-value".to_vec(), None)
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
    let pins = store.pin_secrets(&op, &app).await.unwrap();
    store
        .put_stored_secret(Daemon, app.id(), "token", 1, b"second-value".to_vec(), None)
        .await
        .unwrap();
    assert!(
        store
            .put_stored_secret(Daemon, app.id(), "token", 1, b"stale".to_vec(), None)
            .await
            .is_err()
    );
    assert_eq!(
        store
            .secret_plaintext(&env, &pins["token"])
            .await
            .unwrap()
            .as_slice(),
        b"first-value"
    );
    assert!(matches!(
        store
            .begin_stored_secret_deletion(Daemon, app.id(), "token", 2)
            .await,
        Err(StoreError::SecretReferenced)
    ));
    assert!(
        store
            .secret_plaintext(
                &EnvironmentId::parse("app-another").unwrap(),
                &pins["token"]
            )
            .await
            .is_err()
    );
    drop(store);
    let store = Store::open(&path).await.unwrap();
    assert_eq!(
        store.pin_secrets(&op, &app).await.unwrap(),
        pins,
        "retry must use the pre-rotation pin after restart"
    );
    let snapshot = store.deployment_snapshot(&op.id).await.unwrap();
    for json in [
        serde_json::to_string(&snapshot.template),
        serde_json::to_string(&snapshot.rendering.unwrap().application),
    ] {
        assert!(!json.unwrap().contains("first-value"));
    }
    let next = store.request_deploy(&env, None).await.unwrap();
    let new_pins = store.pin_secrets(&next, &app).await.unwrap();
    assert_ne!(pins, new_pins);
    assert_eq!(
        store
            .secret_plaintext(&env, &new_pins["token"])
            .await
            .unwrap()
            .as_slice(),
        b"second-value"
    );
    std::fs::remove_file(directory.path().join("secrets.key")).unwrap();
    assert_eq!(
        store.stored_secrets(app.id()).await.unwrap()[0]
            .metadata
            .generation,
        2,
        "metadata reads do not require the key"
    );
    assert!(
        store
            .put_stored_secret(Daemon, app.id(), "token", 2, b"replacement".to_vec(), None)
            .await
            .is_err()
    );
    assert!(!directory.path().join("secrets.key").exists());
}

#[tokio::test]
async fn lost_key_recovery_discards_values_until_replacements_are_deployed() {
    let temp = tempfile::tempdir().unwrap();
    let key = temp.path().join("secrets.key");
    let store = Store::open(temp.path().join("db")).await.unwrap();
    let app = with_secret(&application());
    let deployed = store
        .save_application(&ApplicationTemplate::from(&app), None, None)
        .await
        .unwrap();
    store
        .put_stored_secret(Daemon, app.id(), "token", 0, b"original".to_vec(), None)
        .await
        .unwrap();
    let pinned = store.pin_secrets(&deployed, &app).await.unwrap();
    assert!(matches!(
        store.recover_secret_key(Daemon,).await,
        Err(StoreError::SecretKeyUsable)
    ));
    assert_eq!(
        &*store
            .secret_plaintext(&environment(&app), &pinned["token"])
            .await
            .unwrap(),
        b"original"
    );

    std::fs::write(&key, [42; 32]).unwrap();
    let recovery = store.recover_secret_key(Daemon).await.unwrap();
    assert_eq!(
        (
            recovery.affected_environments,
            recovery.affected_applications,
            recovery.affected_secrets,
            recovery.discarded_versions
        ),
        (0, 1, 1, 1)
    );
    assert!(!key.exists());
    let retired = std::fs::read_dir(temp.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.to_string_lossy().contains("secrets.key.retired-"))
        .expect("the unusable key is kept aside");
    assert_eq!(std::fs::read(retired).unwrap(), [42; 32]);
    assert!(
        store.stored_secrets(app.id()).await.unwrap()[0]
            .metadata
            .unavailable
    );
    assert!(matches!(
        store.secret_plaintext(&environment(&app), &pinned["token"]).await,
        Err(StoreError::SecretUnavailable { names }) if names == "token"
    ));
    assert!(matches!(
        store.pin_secrets(&deployed, &app).await,
        Err(StoreError::SecretUnavailable { .. })
    ));

    store
        .put_stored_secret(Daemon, app.id(), "token", 1, b"replacement".to_vec(), None)
        .await
        .unwrap();
    assert!(key.exists(), "the first new value generates a key");
    let redeployed = store
        .save_application(&ApplicationTemplate::from(&app), None, None)
        .await
        .unwrap();
    let pins = store.pin_secrets(&redeployed, &app).await.unwrap();
    assert_eq!(
        &*store
            .secret_plaintext(&environment(&app), &pins["token"])
            .await
            .unwrap(),
        b"replacement"
    );
    assert!(matches!(
        store.recover_secret_key(Daemon,).await,
        Err(StoreError::SecretKeyUsable)
    ));
}

#[tokio::test]
async fn mounted_declared_secrets_are_generated_once_until_regenerated() {
    use piqueld_core::manifest::{SecretDeclaration, SecretEncoding, SecretGenerator, SecretMount};
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    let mut manifest = with_secret(&application()).to_manifest();
    manifest.spec.services[0].secrets.push(SecretMount {
        name: "manual".into(),
        target: "/run/secrets/manual".into(),
    });
    // "unused" is declared but not mounted, so it is not generated.
    for name in ["token", "manual", "unused"] {
        manifest.spec.secrets.push(SecretDeclaration {
            name: name.into(),
            generate: SecretGenerator::Random {
                bytes: 16,
                encoding: SecretEncoding::Hex,
            },
        });
    }
    let app = manifest
        .validate()
        .unwrap()
        .normalize(application().id().clone());
    let first = store
        .save_application(&ApplicationTemplate::from(&app), None, None)
        .await
        .unwrap();
    store
        .put_secret(Daemon, &environment(&app), "manual", 0, b"chosen".to_vec())
        .await
        .unwrap();
    let pins = store.pin_secrets(&first, &app).await.unwrap();
    let value = store
        .secret_plaintext(&environment(&app), &pins["token"])
        .await
        .unwrap();
    assert_eq!(value.len(), 32);

    let second = store
        .save_application(&ApplicationTemplate::from(&app), None, Some(1))
        .await
        .unwrap();
    assert_eq!(store.pin_secrets(&second, &app).await.unwrap(), pins);
    let generations = store
        .secrets(&environment(&app))
        .await
        .unwrap()
        .into_iter()
        .map(|secret| (secret.name, secret.generation))
        .collect::<Vec<_>>();
    assert_eq!(generations, [("manual".into(), 1), ("token".into(), 1)]);

    // Regenerating adds a version for later deployments; pins keep theirs.
    let token = store
        .regenerate_secret(Daemon, &environment(&app), "token", 1)
        .await
        .unwrap();
    assert_eq!(token.generation, 2);
    assert_eq!(store.pin_secrets(&second, &app).await.unwrap(), pins);
    let third = store
        .save_application(&ApplicationTemplate::from(&app), None, Some(2))
        .await
        .unwrap();
    let regenerated = store.pin_secrets(&third, &app).await.unwrap();
    let new_value = store
        .secret_plaintext(&environment(&app), &regenerated["token"])
        .await
        .unwrap();
    assert_ne!(&*new_value, &*value);
    // Only secrets the saved manifest declares can be regenerated.
    assert!(matches!(
        store
            .regenerate_secret(Daemon, &environment(&app), "undeclared", 0)
            .await,
        Err(StoreError::NotFound)
    ));
}

/// Key recovery leaves nothing of a generated value to keep, so the next
/// deployment that mounts it generates a new one. That is the only way back
/// when the manifest declaring it was never accepted, e.g. after a failed
/// repository deployment, since `regenerate_secret` reads the accepted one.
#[tokio::test]
async fn deployments_replace_generated_values_discarded_by_key_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    let accepted = ApplicationTemplate::from(&application());
    let first = store.save_application(&accepted, None, None).await.unwrap();
    let (app, env) = (with_generated_secret(&application()), &first.environment_id);
    store.generate_secrets(env, &app).await.unwrap();
    std::fs::write(temp.path().join("secrets.key"), [42; 32]).unwrap();
    store.recover_secret_key(Daemon).await.unwrap();
    assert!(store.secrets(env).await.unwrap()[0].unavailable);
    assert!(matches!(
        store.regenerate_secret(Daemon, env, "token", 1).await,
        Err(StoreError::NotFound)
    ));

    let second = store
        .save_application(&accepted, None, Some(1))
        .await
        .unwrap();
    let pins = store.pin_secrets(&second, &app).await.unwrap();
    let token = &store.secrets(env).await.unwrap()[0];
    assert_eq!((token.generation, token.unavailable), (2, false));
    let value = store.secret_plaintext(env, &pins["token"]).await.unwrap();
    assert_eq!(value.len(), 32);
}

#[tokio::test]
async fn sibling_environment_deletions_do_not_block_another_environments_deployment() {
    use crate::api::{Mutation, MutationResponse};
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    let app = application();
    let captured = with_generated_secret(&app);
    store
        .save_application(&ApplicationTemplate::from(&captured), None, None)
        .await
        .unwrap();
    let production = environment(&app);
    let (MutationResponse::Environment(staging), _) = store
        .accept(
            Daemon,
            Mutation::CreateEnvironment {
                application: app.id().clone(),
                name: piqueld_core::EnvironmentName::parse("staging").unwrap(),
                branch: None,
            },
            Some(1),
            false,
            None,
        )
        .await
        .unwrap()
    else {
        panic!("environment")
    };
    for environment in [&production, &staging.id] {
        store
            .put_secret(Daemon, environment, "token", 0, b"value".to_vec())
            .await
            .unwrap();
    }
    // Production captures a deployment that mounts the secret, then the shared
    // configuration stops mounting it.
    let deployment = store.request_deploy(&production, Some(2)).await.unwrap();
    store
        .accept(
            Daemon,
            Mutation::Save {
                application: Box::new(ApplicationTemplate::from(&app)),
                expected_application_id: Some(app.id().to_string()),
                deploy: false,
            },
            Some(2),
            false,
            None,
        )
        .await
        .unwrap();
    store
        .begin_secret_deletion(Daemon, &staging.id, "token", 1)
        .await
        .unwrap();
    assert_eq!(
        store
            .pin_secrets(&deployment, &captured)
            .await
            .unwrap()
            .len(),
        1
    );
    // Shared configuration still cannot start using the secret being deleted.
    assert!(matches!(
        store
            .accept(
                Daemon,
                Mutation::Save {
                    application: Box::new(ApplicationTemplate::from(&captured)),
                    expected_application_id: Some(app.id().to_string()),
                    deploy: false,
                },
                Some(3),
                false,
                None,
            )
            .await,
        Err(StoreError::SecretDeleting)
    ));
}

/// Seeds account `dev` with developer access and its API token, and returns
/// the caller acting with that token.
async fn developer(store: &Store) -> crate::store::Actor<'static> {
    use crate::store::{Actor, Caller, CredentialKind, NewCredential};
    use piqueld_core::access::{Preset, Scope};
    let user = piqueld_core::auth::User {
        id: "dev".into(),
        username: "dev".into(),
        display_name: String::new(),
    };
    store
        .seed_auth_user(&user, &Preset::Developer.grants(&Scope::All))
        .await;
    let credential = NewCredential {
        id: "dev-token".into(),
        secret_hash: "hash".into(),
        kind: CredentialKind::Token,
        name: "Test",
        expires_at: None,
        grants: None,
    };
    store
        .insert_credential(&user.id, &credential)
        .await
        .unwrap();
    Actor::Account(Caller {
        credential_id: "dev-token",
        user_id: "dev",
    })
}

/// Demotes `dev` (see `developer`) to read-only access, and returns the
/// refusal its secret writes now get.
async fn demote_developer(store: &Store) -> piqueld_core::access::Denied {
    use piqueld_core::access::{AppPermission, Denied, Permission, Preset, Scope};
    let mut db = store.pool.acquire().await.unwrap();
    super::super::access::Holder::User("dev")
        .replace(&mut db, &Preset::ReadOnly.grants(&Scope::All))
        .await
        .unwrap();
    Denied::Missing(Permission::App(AppPermission::SecretsWrite))
}

/// Kinds of `app`'s events recorded as caused by `dev`.
async fn developer_events(
    store: &Store,
    app: &NormalizedApplication,
) -> std::collections::BTreeSet<String> {
    use crate::store::Visibility;
    let filter = piqueld_core::observability::EventFilter {
        application_id: Some(app.id().to_string()),
        ..Default::default()
    };
    store
        .filtered_events(&filter, &Visibility::ALL, None, 100)
        .await
        .unwrap()
        .items
        .into_iter()
        .filter(|event| {
            matches!(&event.actor, Some(piqueld_core::EventActor::Account { user_id, .. }) if user_id == "dev")
        })
        .map(|event| event.kind)
        .collect()
}

/// Secret writes check the caller's grants inside their transaction, so a
/// caller demoted after authenticating cannot store or delete values. Their
/// events, including the deletion's runtime action, record the caller.
#[tokio::test]
async fn secret_writes_use_the_callers_current_grants() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    let app = application();
    store
        .save_application(&ApplicationTemplate::from(&app), None, None)
        .await
        .unwrap();
    let production = environment(&app);
    let caller = developer(&store).await;
    store
        .put_secret(caller, &production, "token", 0, b"value".to_vec())
        .await
        .unwrap();
    let by = caller.attribution();
    let deletion = store
        .begin_secret_deletion(caller, &production, "token", 1)
        .await
        .unwrap();
    store
        .begin_application_action(by, &production, "remove_secrets", Some("token"))
        .await
        .unwrap();
    store.interrupt_actions(None).await.unwrap();
    store
        .finish_secret_deletion(by, &production, "token", &deletion.id)
        .await
        .unwrap();
    let expected = [
        "secret_saved",
        "action_started",
        "action_outcome_unknown",
        "secret_deleted",
    ];
    assert_eq!(
        developer_events(&store, &app).await,
        expected.map(String::from).into()
    );
    store
        .put_secret(caller, &production, "token", 0, b"value".to_vec())
        .await
        .unwrap();
    let refused = demote_developer(&store).await;
    assert!(matches!(
        store.put_secret(caller, &production, "token", 1, b"again".to_vec()).await,
        Err(StoreError::Denied(denied)) if denied == refused
    ));
    assert!(matches!(
        store.begin_secret_deletion(caller, &production, "token", 1).await,
        Err(StoreError::Denied(denied)) if denied == refused
    ));
}

/// Writes to the application's store check the caller's grants like
/// environment secret writes: a demoted caller cannot store, delete, or
/// change who may mount a value. Their events record the caller.
#[tokio::test]
async fn stored_secret_writes_use_the_callers_current_grants() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    let app = application();
    store
        .save_application(&ApplicationTemplate::from(&app), None, None)
        .await
        .unwrap();
    let caller = developer(&store).await;
    let access = piqueld_core::api::SecretAccess::default();
    store
        .put_stored_secret(caller, app.id(), "stripe", 0, b"value".to_vec(), None)
        .await
        .unwrap();
    store
        .set_secret_access(caller, app.id(), "stripe", &access)
        .await
        .unwrap();
    assert_eq!(
        developer_events(&store, &app).await,
        ["secret_access_changed", "secret_saved"]
            .map(String::from)
            .into()
    );
    let refused = demote_developer(&store).await;
    assert!(matches!(
        store.put_stored_secret(caller, app.id(), "stripe", 1, b"again".to_vec(), None).await,
        Err(StoreError::Denied(denied)) if denied == refused
    ));
    assert!(matches!(
        store.set_secret_access(caller, app.id(), "stripe", &access).await,
        Err(StoreError::Denied(denied)) if denied == refused
    ));
    assert!(matches!(
        store.begin_stored_secret_deletion(caller, app.id(), "stripe", 1).await,
        Err(StoreError::Denied(denied)) if denied == refused
    ));
}
