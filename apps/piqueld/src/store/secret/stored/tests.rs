use super::*;
use crate::api::Actor::Daemon;
use crate::api::{Mutation, MutationResponse};
use piqueld_core::{
    EnvironmentName,
    manifest::{ApplicationManifest, ApplicationTemplate, SecretMount, Variable},
};

/// An application whose `web` service mounts the stored secret its
/// environment's `stripe_key` names (`stripe-dev` unless overridden), and the
/// generated secret `session`.
fn template() -> ApplicationTemplate {
    let mut manifest = piqueld_core::parse_toml(include_str!(
        "../../../../../../crates/piqueld-core/tests/fixtures/manifests/prebuilt.toml"
    ))
    .unwrap()
    .normalize(ApplicationId::parse("app-stored-test").unwrap())
    .to_manifest();
    manifest
        .spec
        .variables
        .insert("stripe_key".into(), Variable::String("stripe-dev".into()));
    manifest.spec.environments.insert(
        "production".into(),
        piqueld_core::manifest::EnvironmentConfig {
            variables: [(
                "stripe_key".to_owned(),
                Variable::String("stripe-prod".into()),
            )]
            .into(),
            ..Default::default()
        },
    );
    manifest.spec.services[0].secrets = vec![
        SecretMount {
            name: "${{ vars.stripe_key }}".into(),
            target: "/run/secrets/stripe".into(),
        },
        SecretMount {
            name: "session".into(),
            target: "/run/secrets/session".into(),
        },
    ];
    manifest.spec.secrets = serde_json::from_value(serde_json::json!([
        {"name":"session", "generate":{"type":"random", "bytes":16}}
    ]))
    .unwrap();
    manifest
        .validate_template()
        .unwrap()
        .normalize(ApplicationId::parse("app-stored-test").unwrap())
}

/// Access limited to `environments`, without previews.
fn only<'a>(environments: impl IntoIterator<Item = &'a EnvironmentId>) -> SecretAccess {
    SecretAccess {
        environments: EnvironmentAccess::Only(environments.into_iter().cloned().collect()),
        previews: false,
    }
}

/// Saves `template()` with a `staging` environment next to `production`.
/// Returns the store's directory, the store, and both environment IDs.
async fn fixture() -> (tempfile::TempDir, Store, EnvironmentId, EnvironmentId) {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    let template = template();
    store.save_application(&template, None, None).await.unwrap();
    let (MutationResponse::Environment(staging), _) = store
        .accept(
            Daemon,
            Mutation::CreateEnvironment {
                application: template.id().clone(),
                name: EnvironmentName::parse("staging").unwrap(),
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
    let production = EnvironmentId::default_for(template.id());
    (temp, store, production, staging.id)
}

/// Captures a deployment of `environment` and pins its secrets.
async fn deploy(
    store: &Store,
    environment: &EnvironmentId,
) -> Result<BTreeMap<String, String>, StoreError> {
    let operation = store.request_deploy(environment, None).await?;
    let rendered = store
        .deployment_snapshot(&operation.id)
        .await?
        .rendering
        .unwrap()
        .application;
    store.pin_secrets(&operation, &rendered).await
}

#[tokio::test]
async fn environments_mount_the_stored_secret_their_variables_name_where_access_allows() {
    let (_temp, store, production, staging) = fixture().await;
    let application = template().id().clone();
    store
        .put_stored_secret(
            Daemon,
            &application,
            "stripe-prod",
            0,
            b"live".to_vec(),
            Some(&only([&production])),
        )
        .await
        .unwrap();
    store
        .put_stored_secret(
            Daemon,
            &application,
            "stripe-dev",
            0,
            b"test".to_vec(),
            None,
        )
        .await
        .unwrap();

    // Each environment mounts its own stored secret through `stripe_key`, and
    // its own generated `session`.
    let production_pins = deploy(&store, &production).await.unwrap();
    let staging_pins = deploy(&store, &staging).await.unwrap();
    for (environment, pins, value) in [
        (&production, &production_pins, &b"live"[..]),
        (&staging, &staging_pins, &b"test"[..]),
    ] {
        let stripe = pins
            .iter()
            .find(|(name, _)| name.starts_with("stripe"))
            .unwrap()
            .1;
        assert_eq!(
            &*store.secret_plaintext(environment, stripe).await.unwrap(),
            value
        );
        assert_eq!(store.secrets(environment).await.unwrap()[0].name, "session");
    }
    assert_eq!(
        production_pins.keys().collect::<Vec<_>>(),
        ["session", "stripe-prod"]
    );
    assert_eq!(
        staging_pins.keys().collect::<Vec<_>>(),
        ["session", "stripe-dev"]
    );

    // A secret every environment may mount gets one Docker secret per environment.
    let mut manifest = template().to_manifest();
    manifest.spec.environments.clear();
    let shared = manifest
        .validate_template()
        .unwrap()
        .normalize(application.clone());
    store
        .accept(
            Daemon,
            Mutation::Save {
                application: Box::new(shared),
                expected_application_id: Some(application.to_string()),
                deploy: false,
            },
            None,
            true,
            None,
        )
        .await
        .unwrap();
    let production_copy = deploy(&store, &production).await.unwrap()["stripe-dev"].clone();
    assert_ne!(production_copy, staging_pins["stripe-dev"]);
    assert_eq!(
        &*store
            .secret_plaintext(&production, &production_copy)
            .await
            .unwrap(),
        b"test"
    );
    assert!(
        store
            .secret_plaintext(&production, &staging_pins["stripe-dev"])
            .await
            .is_err(),
        "an environment cannot read another environment's Docker secret"
    );
}

#[tokio::test]
async fn capture_and_pinning_deny_stored_secrets_outside_the_access_list() {
    let (_temp, store, production, staging) = fixture().await;
    let application = template().id().clone();
    store
        .put_stored_secret(
            Daemon,
            &application,
            "stripe-dev",
            0,
            b"test".to_vec(),
            None,
        )
        .await
        .unwrap();
    // Captured while staging may still mount it, pinned after access narrowed.
    let captured = store.request_deploy(&staging, None).await.unwrap();
    store
        .set_secret_access(Daemon, &application, "stripe-dev", &only([&production]))
        .await
        .unwrap();
    let denied = |result: Result<_, StoreError>| {
        matches!(
            result,
            Err(StoreError::SecretAccessDenied { environment, secret })
                if environment.as_str() == "staging" && secret == "stripe-dev"
        )
    };
    assert!(denied(
        store.request_deploy(&staging, None).await.map(|_| ())
    ));
    let rendered = store
        .deployment_snapshot(&captured.id)
        .await
        .unwrap()
        .rendering
        .unwrap()
        .application;
    assert!(denied(
        store.pin_secrets(&captured, &rendered).await.map(|_| ())
    ));
    let environment = store.get(&staging).await.unwrap().environment;
    assert!(denied(
        store.check_secret_access(&environment, &rendered).await
    ));
}

#[tokio::test]
async fn a_name_is_either_declared_and_generated_or_stored() {
    let (_temp, store, _, _) = fixture().await;
    let application = template().id().clone();
    let conflict = |result: Result<(), StoreError>| match result {
        Err(StoreError::Validation(errors)) => {
            errors.0.len() == 1
                && errors.0[0].code == codes::SECRET_NAME_CONFLICT
                && errors.0[0].path == "spec.secrets[0].name"
        }
        _ => false,
    };
    assert!(conflict(
        store
            .put_stored_secret(Daemon, &application, "session", 0, b"manual".to_vec(), None)
            .await
            .map(|_| ())
    ));
    store
        .put_stored_secret(Daemon, &application, "api-key", 0, b"manual".to_vec(), None)
        .await
        .unwrap();
    let mut manifest = template().to_manifest();
    manifest.spec.secrets[0].name = "api-key".into();
    manifest.spec.services[0].secrets[1].name = "api-key".into();
    let declared = manifest
        .validate_template()
        .unwrap()
        .normalize(application.clone());
    assert!(conflict(
        store
            .accept(
                Daemon,
                Mutation::Save {
                    application: Box::new(declared),
                    expected_application_id: Some(application.to_string()),
                    deploy: false,
                },
                None,
                true,
                None,
            )
            .await
            .map(|_| ())
    ));
}

#[tokio::test]
async fn access_lists_keep_renamed_environments_and_drop_deleted_ones() {
    let (_temp, store, production, staging) = fixture().await;
    let application = template().id().clone();
    let stored = store
        .put_stored_secret(
            Daemon,
            &application,
            "stripe-dev",
            0,
            b"test".to_vec(),
            Some(&only([&staging])),
        )
        .await
        .unwrap();
    assert_eq!(stored.access, only([&staging]));

    // A renamed environment keeps its access and is shown by its new name.
    store
        .accept(
            Daemon,
            Mutation::RenameEnvironment {
                id: staging.clone(),
                name: EnvironmentName::parse("qa").unwrap(),
            },
            None,
            true,
            None,
        )
        .await
        .unwrap();
    let access = store.stored_secrets(&application).await.unwrap()[0]
        .access
        .clone();
    assert_eq!(access, only([&staging]));
    let environments = store.environments(&application).await.unwrap();
    assert_eq!(access.describe(&environments), "qa");
    deploy(&store, &staging).await.unwrap();

    // Deleting an environment removes it from every list. Every-environment
    // access is unaffected, and covers environments created later.
    store
        .put_stored_secret(Daemon, &application, "shared", 0, b"all".to_vec(), None)
        .await
        .unwrap();
    let deletion = store.request_delete(&staging).await.unwrap();
    sqlx::query!(
        "UPDATE operations SET state='running' WHERE id=?1",
        deletion.id
    )
    .execute(&store.pool)
    .await
    .unwrap();
    store.finish_delete_operation(&deletion).await.unwrap();
    let secrets = store.stored_secrets(&application).await.unwrap();
    assert_eq!(secrets[0].metadata.name, "shared");
    assert_eq!(secrets[0].access, SecretAccess::default());
    assert_eq!(secrets[1].access, only([]));
    assert_eq!(secrets[1].access.describe(&[]), "no environment");
    assert!(!secrets[1].access.allows(&production));
    assert!(matches!(
        store
            .set_secret_access(Daemon, &application, "shared", &only([&staging]))
            .await,
        Err(StoreError::InvalidInput)
    ));
}

/// A new database `db` in `dir` at schema 20, which predates application stores.
async fn schema_20(dir: &std::path::Path) -> sqlx::SqlitePool {
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(dir.join("db"))
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    for (index, migration) in crate::store::MIGRATIONS.iter().take(20).enumerate() {
        Store::apply_migration(&pool, index + 1, migration)
            .await
            .unwrap();
    }
    pool
}

/// A schema-20 database `db` in `dir` with application `app-migrated-01`: its
/// production environment has the manually set `stripe` (versions 1 and 2,
/// with the last deployment pinning version 1), the generated `session`,
/// `retired`, which only that deployment still declares and pins, and
/// `recreated`, which it declares but no longer pins: it was deleted and set
/// manually since. It also has a manually set `branch-token`, which staging's
/// manifest, last fetched from the `release` branch, declares. Staging has its
/// own `stripe`, and a manually set `shared-token`, which production's saved
/// manifest declares though production never generated it. Returns the
/// application and both environments.
async fn schema_20_with_manual_secrets(
    dir: &std::path::Path,
) -> (NormalizedApplication, EnvironmentId, EnvironmentId) {
    use crate::secrets::{SecretCipher, SecretOwner};
    let pool = schema_20(dir).await;
    // `stripe` is set manually; `session` is declared, so it was generated.
    let mut manifest = template().to_manifest();
    manifest.spec.variables.clear();
    manifest.spec.environments.clear();
    manifest.spec.services[0].secrets[0].name = "stripe".into();
    let application = manifest
        .clone()
        .validate()
        .unwrap()
        .normalize(ApplicationId::parse("app-migrated-01").unwrap());
    let (id, production, staging) = (
        application.id().as_str(),
        EnvironmentId::default_for(application.id()),
        EnvironmentId::parse("env-staging-01").unwrap(),
    );
    let declared = |manifest: &ApplicationManifest, names: &[&str]| {
        let mut manifest = manifest.clone();
        for name in names {
            let mut secret = manifest.spec.secrets[0].clone();
            secret.name = (*name).into();
            manifest.spec.secrets.push(secret);
        }
        let application = manifest.validate().unwrap();
        ApplicationTemplate::from(&application.normalize(ApplicationId::parse(id).unwrap()))
            .canonical_json()
            .unwrap()
    };
    let desired = declared(&manifest, &["shared-token"]);
    let fetched = declared(&manifest, &["branch-token"]);
    let captured = declared(&manifest, &["retired", "recreated"]);
    sqlx::raw_sql(&format!(
        "INSERT INTO applications(id,name,desired_json,generation,created_at_ms,updated_at_ms) VALUES('{id}','notes','{desired}',2,1,1);
         INSERT INTO environments(id,name,application_id,created_at_ms,updated_at_ms) VALUES('{id}','production','{id}',1,1);
         INSERT INTO environments(id,name,application_id,branch,manifest_json,created_at_ms,updated_at_ms) VALUES('{staging}','staging','{id}','release','{fetched}',2,2);
         INSERT INTO environment_status(environment_id,state,updated_at_ms) VALUES('{id}','ready',1),('{staging}','ready',2);
         INSERT INTO operations(id,environment_id,kind,state,generation,created_at_ms,updated_at_ms,finished_at_ms) VALUES('operation-1','{id}','apply','succeeded',2,1,2,2);
         INSERT INTO deployments(id,environment_id,manifest_json,template_json,generation,created_at_ms) VALUES('operation-1','{id}','{captured}','{captured}',2,1);
         INSERT INTO deployment_secrets_prepared(operation_id) VALUES('operation-1');"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let cipher = SecretCipher::load(&dir.join("secrets.key"), false).unwrap();
    let verifier = cipher
        .encrypt(
            SecretOwner::KeyVerifier,
            "key-verification",
            1,
            b"piqueld-secret-key-v1",
        )
        .unwrap();
    sqlx::query("INSERT INTO secret_key_verification(singleton,nonce,ciphertext) VALUES(1,?1,?2)")
        .bind(verifier.nonce)
        .bind(verifier.ciphertext)
        .execute(&pool)
        .await
        .unwrap();
    for (environment, name, generation, value) in [
        (&production, "stripe", 1, "first"),
        (&production, "stripe", 2, "second"),
        (&production, "session", 1, "generated"),
        (&production, "retired", 1, "retired"),
        (&production, "recreated", 1, "recreated"),
        (&production, "branch-token", 1, "branch"),
        (&staging, "stripe", 1, "staging"),
        (&staging, "shared-token", 1, "shared"),
    ] {
        let encrypted = cipher
            .encrypt(
                SecretOwner::Environment(environment),
                name,
                generation,
                value.as_bytes(),
            )
            .unwrap();
        sqlx::query("INSERT INTO environment_secrets(environment_id,name,generation,updated_at_ms) VALUES(?1,?2,?3,1) ON CONFLICT DO UPDATE SET generation=excluded.generation")
            .bind(environment.as_str()).bind(name).bind(generation).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO secret_versions(environment_id,name,generation,swarm_name,nonce,ciphertext) VALUES(?1,?2,?3,?4,?5,?6)")
            .bind(environment.as_str()).bind(name).bind(generation).bind(format!("docker-{value}"))
            .bind(encrypted.nonce).bind(encrypted.ciphertext).execute(&pool).await.unwrap();
    }
    // The last deployment pinned the first version of `stripe`.
    sqlx::query("INSERT INTO deployment_secret_pins(operation_id,environment_id,name,generation) VALUES('operation-1',?1,'stripe',1),('operation-1',?1,'session',1),('operation-1',?1,'retired',1)")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    (application, production, staging)
}

#[tokio::test]
async fn migration_moves_manual_secrets_to_the_application_store_and_keeps_pins() {
    let temp = tempfile::tempdir().unwrap();
    let (application, production, staging) = schema_20_with_manual_secrets(temp.path()).await;

    let store = Store::open(&temp.path().join("db")).await.unwrap();
    // Production's manual secrets moved with access limited to production.
    let stored = store.stored_secrets(application.id()).await.unwrap();
    assert_eq!(
        stored
            .iter()
            .map(|secret| (secret.metadata.name.as_str(), secret.metadata.generation))
            .collect::<Vec<_>>(),
        [("branch-token", 1), ("recreated", 1), ("stripe", 2)]
    );
    assert!(
        stored
            .iter()
            .all(|secret| secret.access == only([&production]))
    );
    // Generated secrets stay, including ones only a retained deployment
    // declares and pins. Production decides names both environments use: its
    // manual `branch-token` moved though staging declares it, and staging's
    // `shared-token` stays, since production declares it. Staging's
    // same-named `stripe` stays with staging.
    let names = |secrets: Vec<SecretMetadata>| {
        secrets
            .into_iter()
            .map(|secret| secret.name)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(store.secrets(&production).await.unwrap()),
        ["retired", "session"]
    );
    assert_eq!(
        names(store.secrets(&staging).await.unwrap()),
        ["shared-token", "stripe"]
    );
    // Moved values are re-encrypted for the application and still decrypt,
    // under their original Docker secret names.
    let moved: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM application_secret_versions WHERE moved_from IS NOT NULL",
    )
    .fetch_one(&store.pool)
    .await
    .unwrap();
    assert_eq!(moved, 0);
    for (environment, swarm_name, value) in [
        (&production, "docker-first", "first"),
        (&production, "docker-second", "second"),
        (&production, "docker-generated", "generated"),
        (&production, "docker-retired", "retired"),
        (&production, "docker-recreated", "recreated"),
        (&production, "docker-branch", "branch"),
        (&staging, "docker-staging", "staging"),
        (&staging, "docker-shared", "shared"),
    ] {
        assert_eq!(
            &*store
                .secret_plaintext(environment, swarm_name)
                .await
                .unwrap(),
            value.as_bytes()
        );
    }
    // The retained deployment still resolves to the same Docker secrets.
    let operation = store
        .latest_operation_for_environment(&production)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        store.pin_secrets(&operation, &application).await.unwrap(),
        BTreeMap::from([
            ("retired".to_owned(), "docker-retired".to_owned()),
            ("session".to_owned(), "docker-generated".to_owned()),
            ("stripe".to_owned(), "docker-first".to_owned()),
        ])
    );
}
