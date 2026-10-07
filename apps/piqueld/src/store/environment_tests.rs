//! Upgrade coverage for environment identity, captured targets and encrypted secrets.
use super::*;
use crate::api;
use piqueld_core::manifest::ApplicationTemplate;

struct LegacyApplication {
    id: ApplicationId,
    manifest: NormalizedApplication,
    resolved: ResolvedApplication,
    ciphertext: Vec<u8>,
}

impl LegacyApplication {
    const SECRET_NAME: &'static str = "piqueld-secret-legacy";

    async fn insert(pool: &SqlitePool, key_path: &Path) -> Self {
        let id = ApplicationId::parse("app-legacy-01").unwrap();
        let manifest = piqueld_core::parse_toml(&format!(
            "{}\n[[spec.services.secrets]]\nname='token'\ntarget='/run/secrets/token'\n[[spec.routes]]\nhostname='notes.example.com'\nservice='web'\nport=80",
            include_str!("../../../../crates/piqueld-core/tests/fixtures/manifests/prebuilt.toml")
        ))
        .unwrap()
        .normalize(id.clone());
        let desired = serde_json::to_string(&manifest).unwrap();
        sqlx::raw_sql(&format!(
            "INSERT INTO applications(id,name,desired_json,generation,created_at_ms,updated_at_ms) VALUES('{id}','notes','{desired}',3,1,2);
             INSERT INTO application_status(application_id,state,updated_at_ms) VALUES('{id}','ready',2);
             INSERT INTO operations(id,application_id,kind,state,generation,created_at_ms,updated_at_ms,finished_at_ms) VALUES('operation-1','{id}','refresh','succeeded',3,1,2,2);
             INSERT INTO events(application_id,operation_id,kind,created_at_ms) VALUES('{id}','operation-1','operation_succeeded',2);
             INSERT INTO request_receipts VALUES('legacy-deploy','fingerprint','{{\"Operation\":{{\"operation_id\":\"operation-1\",\"application_id\":\"{id}\",\"generation\":3}}}}',9000000000000000);"
        ))
        .execute(pool)
        .await
        .unwrap();
        let secret_name = Self::SECRET_NAME;
        let ciphertext = Self::insert_secret(pool, key_path, &id).await;
        let instance: String =
            sqlx::query_scalar("SELECT instance_id FROM instance_metadata WHERE singleton=1")
                .fetch_one(pool)
                .await
                .unwrap();
        let resolved = piqueld_core::compile_application(
            &manifest,
            &EnvironmentId::default_for(&id),
            piqueld_core::InstanceId::parse(instance).unwrap(),
            &piqueld_core::ResolutionSet {
                sources: [(
                    piqueld_core::ServiceName::parse("web").unwrap(),
                    piqueld_core::ResolvedSource::parse_image(
                        "ghcr.io/example/notes:1.4.0",
                        format!("ghcr.io/example/notes@sha256:{}", "a".repeat(64)),
                    )
                    .unwrap(),
                )]
                .into(),
                secret_names: [("token".into(), secret_name.into())].into(),
            },
        )
        .unwrap();
        let target = serde_json::to_string(&resolved).unwrap();
        let routes = serde_json::to_string(&manifest.spec().routes).unwrap();
        sqlx::query("UPDATE applications SET resolved_json=?1,resolved_generation=3 WHERE id=?2")
            .bind(&target)
            .bind(id.as_str())
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("UPDATE operations SET target_json=?1,promoted=1 WHERE id='operation-1'")
            .bind(&target)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO deployments(id,application_id,manifest_json,generation,created_at_ms,succeeded_at_ms) VALUES('operation-1',?1,?2,3,1,2)").bind(id.as_str()).bind(&desired).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO deployment_secret_pins(operation_id,application_id,name,generation) VALUES('operation-1',?1,'token',1)").bind(id.as_str()).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO deployment_secrets_prepared(operation_id) VALUES('operation-1')")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO application_routes(application_id,desired_json,applied_json) VALUES(?1,?2,?2)").bind(id.as_str()).bind(routes).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO hostname_reservations(hostname,application_id) VALUES('notes.example.com',?1)").bind(id.as_str()).execute(pool).await.unwrap();
        Self {
            id,
            manifest,
            resolved,
            ciphertext,
        }
    }

    async fn insert_secret(pool: &SqlitePool, key_path: &Path, id: &ApplicationId) -> Vec<u8> {
        let cipher = crate::secrets::SecretCipher::load(key_path, false).unwrap();
        let encrypted = cipher
            .encrypt(id.as_str(), "token", 1, b"legacy secret")
            .unwrap();
        let verifier = cipher
            .encrypt("piqueld", "key-verification", 1, b"piqueld-secret-key-v1")
            .unwrap();
        sqlx::query("INSERT INTO application_secrets(application_id,name,generation,updated_at_ms) VALUES(?1,'token',1,2)")
            .bind(id.as_str()).execute(pool).await.unwrap();
        let secret_name = Self::SECRET_NAME;
        sqlx::query("INSERT INTO secret_versions(application_id,name,generation,swarm_name,nonce,ciphertext) VALUES(?1,'token',1,?2,?3,?4)")
            .bind(id.as_str()).bind(secret_name).bind(&encrypted.nonce).bind(&encrypted.ciphertext).execute(pool).await.unwrap();
        sqlx::query(
            "INSERT INTO secret_key_verification(singleton,nonce,ciphertext) VALUES(1,?1,?2)",
        )
        .bind(verifier.nonce)
        .bind(verifier.ciphertext)
        .execute(pool)
        .await
        .unwrap();
        encrypted.ciphertext
    }

    async fn assert_runtime(
        &self,
        store: &Store,
        environment: &EnvironmentId,
        operation: &Operation,
    ) {
        let resolved = self.resolved.clone();
        let manifest = self.manifest.clone();
        let secret_name = Self::SECRET_NAME;
        // Runtime names, captured inputs, reservations and encryption context
        // survive the split; retrying the deployment uses the same secret version.
        assert_eq!(
            store.get(environment).await.unwrap().resolved.unwrap(),
            resolved
        );
        assert_eq!(
            store.prepared_target(&operation.id).await.unwrap().unwrap(),
            resolved
        );
        let snapshot = store.deployment_snapshot(&operation.id).await.unwrap();
        assert_eq!(snapshot.template, ApplicationTemplate::from(&manifest));
        assert_eq!(snapshot.rendering.unwrap().application, manifest);
        let deployments = store.deployments(environment, None, 10).await.unwrap();
        assert_eq!(deployments.items[0].application.as_ref(), Some(&manifest));
        assert_eq!(
            store.applied_routes(environment).await.unwrap(),
            manifest.spec().routes
        );
        assert_eq!(
            store.routing_table().await.unwrap()[environment],
            manifest.spec().routes
        );
        let owner: String = sqlx::query_scalar(
            "SELECT environment_id FROM hostname_reservations WHERE hostname='notes.example.com'",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(owner, environment.as_str());
        assert_eq!(store.secrets(environment).await.unwrap()[0].generation, 1);
        assert_eq!(
            &*store
                .secret_plaintext(environment, secret_name)
                .await
                .unwrap(),
            b"legacy secret"
        );
        assert_eq!(
            store.pin_secrets(operation, &manifest).await.unwrap()["token"],
            secret_name
        );
        let ciphertext: Vec<u8> =
            sqlx::query_scalar("SELECT ciphertext FROM secret_versions WHERE swarm_name=?1")
                .bind(secret_name)
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(
            ciphertext, self.ciphertext,
            "upgrade leaves existing ciphertext untouched"
        );
    }
}

#[tokio::test]
async fn existing_applications_become_one_production_environment_with_the_same_id() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("upgrade.db");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    // Legacy rows predate environments, migration 12.
    let before = 11;
    for (index, migration) in MIGRATIONS.iter().take(before).enumerate() {
        Store::apply_migration(&pool, index + 1, migration)
            .await
            .unwrap();
    }
    let legacy = LegacyApplication::insert(&pool, &directory.path().join("secrets.key")).await;
    let id = legacy.id.clone();
    let manifest = legacy.manifest.clone();
    pool.close().await;

    let store = Store::open(&path).await.unwrap();
    let application = store.application(&id).await.unwrap();
    assert_eq!(
        application.application,
        ApplicationTemplate::from(&manifest)
    );
    assert_eq!(application.generation, 3);
    let environments = store.environments(&id).await.unwrap();
    assert_eq!(environments.len(), 1);
    let environment = &environments[0];
    assert_eq!(environment.id, EnvironmentId::default_for(&id));
    assert_eq!(environment.name.as_str(), "production");
    assert_eq!(environment.source, EnvironmentSource::Saved);
    assert_eq!(
        store.status(&environment.id).await.unwrap().state,
        ApplicationState::Ready
    );
    let operation = store
        .latest_operation_for_environment(&environment.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(operation.id, "operation-1");
    assert_eq!(operation.generation, 3);
    let events = store.events(Some(&environment.id), None, 10).await.unwrap();
    assert_eq!(events.items.len(), 1);
    assert_eq!(
        events.items[0].application_id.as_ref(),
        Some(&environment.application_id)
    );
    legacy
        .assert_runtime(&store, &environment.id, &operation)
        .await;

    // Deletion also removes receipts accepted before environments existed.
    let (api::MutationResponse::Deleted(deleted), _) = store
        .accept(
            api::Mutation::DeleteApplication {
                id: id.clone(),
                environments: Vec::new(),
            },
            Some(3),
            false,
            None,
        )
        .await
        .unwrap()
    else {
        panic!("deleted")
    };
    let deletion = &deleted.operations[0].operation_id;
    store
        .transition_operation(
            deletion,
            OperationState::Requested,
            OperationState::Running,
            None,
        )
        .await
        .unwrap();
    store
        .finish_delete_operation(&store.operation(deletion).await.unwrap())
        .await
        .unwrap();
    assert!(matches!(
        store.application(&id).await,
        Err(StoreError::NotFound)
    ));
    let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_receipts")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert_eq!(receipts, 0);
}

/// Opens a database migrated to `version`, before the migrations after it.
async fn database_at(path: &Path, version: usize) -> SqlitePool {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    for (index, migration) in MIGRATIONS.iter().take(version).enumerate() {
        Store::apply_migration(&pool, index + 1, migration)
            .await
            .unwrap();
    }
    pool
}

/// Environments of repository-backed applications follow the branch and
/// pinned commit `spec.manifest` named, keeping the manifest last fetched for
/// the application; other environments keep deploying the saved manifest.
/// Stored environment responses take the migrated source too.
#[tokio::test]
async fn repository_backed_environments_take_their_branch_from_the_connection() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("upgrade.db");
    let pool = database_at(&path, 13).await;
    let commit = "c".repeat(40);
    let backed = piqueld_core::manifest::parse_template_toml(&format!(
        "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='notes'\n[spec.manifest]\npath='app.toml'\n[spec.manifest.repository]\nurl='https://example.com/notes.git'\nbranch='release'\ncommit='{commit}'"
    ))
    .unwrap()
    .normalize(ApplicationId::parse("app-backed-01").unwrap());
    let saved = piqueld_core::manifest::parse_template_toml(
        "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='journal'\n[spec]",
    )
    .unwrap()
    .normalize(ApplicationId::parse("app-saved-01").unwrap());
    for (application, environments) in [
        (&backed, ["app-backed-01", "env-backed-staging"]),
        (&saved, ["app-saved-01", "env-saved-staging"]),
    ] {
        let id = application.id().as_str();
        let name = application.metadata().name.as_str();
        sqlx::query("INSERT INTO applications(id,name,desired_json,generation,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,1,1,1)")
            .bind(id).bind(name).bind(application.canonical_json().unwrap())
            .execute(&pool).await.unwrap();
        for (environment, name) in environments.into_iter().zip(["production", "staging"]) {
            sqlx::query("INSERT INTO environments(id,application_id,name,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,1,1)")
                .bind(environment).bind(id).bind(name)
                .execute(&pool).await.unwrap();
        }
    }
    // Receipts of environments created before the migration.
    for (environment, application, source) in [
        ("env-backed-staging", "app-backed-01", "repository"),
        ("env-saved-staging", "app-saved-01", "saved"),
    ] {
        let response = serde_json::json!({"Environment": {
            "id": environment, "application_id": application, "name": "staging",
            "source": source, "resolved_generation": null, "delete_intent": false,
            "created_at_ms": 1, "updated_at_ms": 1,
        }});
        sqlx::query("INSERT INTO request_receipts(request_id,fingerprint,response_json,expires_at_ms) VALUES(?1,'f',?2,?3)")
            .bind(environment).bind(response.to_string()).bind(i64::MAX)
            .execute(&pool).await.unwrap();
    }
    pool.close().await;

    let store = Store::open(&path).await.unwrap();
    let release = TrackedBranch::new("release".into(), Some(commit)).unwrap();
    let replayed = async |request: &str| {
        let response: String = sqlx::query_scalar(
            "SELECT json_extract(response_json,'$.Environment') FROM request_receipts WHERE request_id=?1",
        )
        .bind(request)
        .fetch_one(&store.pool)
        .await
        .unwrap();
        serde_json::from_str::<piqueld_core::api::EnvironmentView>(&response)
            .unwrap()
            .source
    };
    assert_eq!(
        replayed("env-backed-staging").await,
        EnvironmentSource::Branch(release.clone())
    );
    assert_eq!(
        replayed("env-saved-staging").await,
        EnvironmentSource::Saved
    );
    for id in ["app-backed-01", "env-backed-staging"] {
        let environment = store.get(&EnvironmentId::parse(id).unwrap()).await.unwrap();
        assert_eq!(
            environment.environment.source,
            EnvironmentSource::Branch(release.clone())
        );
        assert_eq!(environment.manifest(), Some(&backed));
        assert_eq!(environment.application.application, backed);
    }
    for id in ["app-saved-01", "env-saved-staging"] {
        let environment = store.get(&EnvironmentId::parse(id).unwrap()).await.unwrap();
        assert_eq!(environment.environment.source, EnvironmentSource::Saved);
        assert_eq!(environment.fetched, None);
        assert_eq!(environment.manifest(), Some(&saved));
    }
}
