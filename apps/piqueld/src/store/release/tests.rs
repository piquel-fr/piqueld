use super::*;
use crate::api::Actor::Daemon;
use crate::api::{Mutation, MutationResponse};
use piqueld_core::{
    EnvironmentName, InstanceId, NormalizedApplication, OperationState, ResolutionSet,
    ResolvedSource, ServiceName, compile_application, manifest::ApplicationTemplate,
};
use std::collections::BTreeMap;

const IMAGE: &str = "ghcr.io/example/notes:1.4.0";

/// The `notes` application pulling `web` from [`IMAGE`].
fn notes(id: &str) -> NormalizedApplication {
    piqueld_core::parse_toml(include_str!(
        "../../../../../crates/piqueld-core/tests/fixtures/manifests/prebuilt.toml"
    ))
    .unwrap()
    .normalize(ApplicationId::parse(id).unwrap())
}

/// `IMAGE` resolved to a digest of 64 `byte`s.
fn pulled(byte: char) -> ResolvedSource {
    ResolvedSource::parse_image(
        IMAGE,
        format!(
            "ghcr.io/example/notes@sha256:{}",
            byte.to_string().repeat(64)
        ),
    )
    .unwrap()
}

/// `application` compiled for `environment`, with `web` pulled as `web`.
fn target(
    application: &NormalizedApplication,
    environment: &EnvironmentId,
    instance: &str,
    web: ResolvedSource,
) -> ResolvedApplication {
    compile_application(
        application,
        environment,
        InstanceId::parse(instance).unwrap(),
        &ResolutionSet {
            sources: BTreeMap::from([(ServiceName::parse("web").unwrap(), web)]),
            secret_names: BTreeMap::new(),
        },
    )
    .unwrap()
}

/// Saves `notes` with a `staging` environment next to `production`.
async fn save_with_staging(store: &Store) -> (ApplicationId, EnvironmentId, EnvironmentId) {
    let template = ApplicationTemplate::from(&notes("app-notes-test"));
    let (MutationResponse::Saved(saved), _) = store
        .accept(
            Daemon,
            Mutation::save(
                template.to_manifest().validate_template().unwrap(),
                None,
                false,
            ),
            Some(0),
            false,
            None,
        )
        .await
        .unwrap()
    else {
        panic!("saved")
    };
    let application = ApplicationId::parse(saved.application_id.as_str()).unwrap();
    let create = Mutation::CreateEnvironment {
        application: application.clone(),
        name: EnvironmentName::parse("staging").unwrap(),
        source: None,
    };
    let (MutationResponse::Environment(staging), _) = store
        .accept(Daemon, create, None, true, None)
        .await
        .unwrap()
    else {
        panic!("environment")
    };
    let production = EnvironmentId::default_for(&application);
    (application, production, staging.id)
}

/// Deploys `environment` and prepares it with `web` pulled as `web`,
/// returning the deployment's ID.
async fn prepare(store: &Store, environment: &EnvironmentId, web: ResolvedSource) -> String {
    let operation = store.request_deploy(environment, None).await.unwrap();
    store
        .transition_operation(
            &operation.id,
            OperationState::Requested,
            OperationState::Running,
            None,
        )
        .await
        .unwrap();
    let rendering = store
        .deployment_snapshot(&operation.id)
        .await
        .unwrap()
        .rendering
        .unwrap();
    let target = target(
        &rendering.application,
        environment,
        store.instance_id(),
        web,
    );
    store.save_prepared(&operation, &target).await.unwrap();
    operation.id
}

/// The release each of `environment`'s deployments references, by deployment ID.
async fn deployment_releases(
    store: &Store,
    environment: &EnvironmentId,
) -> BTreeMap<String, Option<ReleaseId>> {
    store
        .deployments(environment, None, 10)
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|deployment| (deployment.operation.id, deployment.release))
        .collect()
}

#[tokio::test]
async fn identical_preparations_share_a_release_that_outlives_its_environment() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    let (application, production, staging) = save_with_staging(&store).await;
    let first = prepare(&store, &production, pulled('a')).await;
    let shared = prepare(&store, &staging, pulled('a')).await;
    let updated = prepare(&store, &production, pulled('b')).await;

    let releases = store.releases(&application, None, 10).await.unwrap().items;
    let [newer, older] = releases.as_slice() else {
        panic!("two releases, got {releases:?}")
    };
    let web = ServiceName::parse("web").unwrap();
    assert_eq!(older.release.sources()[&web], pulled('a'));
    assert_eq!(newer.release.sources()[&web], pulled('b'));
    assert_eq!(older.release.commit(), None);
    assert_eq!(
        older.fingerprint.services()[&web],
        piqueld_core::BuildInputs::new(&pulled('a').requested(), None)
    );
    assert_eq!(older.content_hash, older.release.content_hash());
    let production_releases = deployment_releases(&store, &production).await;
    assert_eq!(production_releases[&first].as_ref(), Some(&older.id));
    assert_eq!(production_releases[&updated].as_ref(), Some(&newer.id));
    assert_eq!(
        deployment_releases(&store, &staging).await[&shared].as_ref(),
        Some(&older.id)
    );

    let deletion = store.request_delete(&staging).await.unwrap();
    store
        .transition_operation(
            &deletion.id,
            OperationState::Requested,
            OperationState::Running,
            None,
        )
        .await
        .unwrap();
    store.finish_delete_operation(&deletion).await.unwrap();
    assert!(matches!(
        store.get(&staging).await,
        Err(StoreError::NotFound)
    ));
    let remaining = store.releases(&application, None, 10).await.unwrap().items;
    assert_eq!(
        remaining
            .iter()
            .map(|release| &release.id)
            .collect::<Vec<_>>(),
        [&newer.id, &older.id]
    );
}

/// A schema-21 database `db` in `dir`: `notes` with `production` and
/// `staging` environments, each with a deployment prepared with `web` pulled
/// at the same digest, and production's newer deployment, never prepared.
/// Production's prepared deployment is its current target.
async fn schema_21_with_prepared_deployments(dir: &std::path::Path) -> ApplicationId {
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(dir.join("db"))
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    for (index, migration) in crate::store::MIGRATIONS.iter().take(21).enumerate() {
        Store::apply_migration(&pool, index + 1, migration)
            .await
            .unwrap();
    }
    let instance: String =
        sqlx::query_scalar("SELECT instance_id FROM instance_metadata WHERE singleton=1")
            .fetch_one(&pool)
            .await
            .unwrap();
    let application = notes("app-migrated-01");
    let (id, production, staging) = (
        application.id().as_str(),
        EnvironmentId::default_for(application.id()),
        EnvironmentId::parse("env-staging-01").unwrap(),
    );
    let template = ApplicationTemplate::from(&application)
        .canonical_json()
        .unwrap();
    let manifest = application.canonical_json().unwrap();
    sqlx::raw_sql(&format!(
        "INSERT INTO applications(id,name,desired_json,generation,created_at_ms,updated_at_ms) VALUES('{id}','notes','{template}',1,1,1);
         INSERT INTO environments(id,name,application_id,created_at_ms,updated_at_ms) VALUES('{id}','production','{id}',1,1),('{staging}','staging','{id}',1,1);
         INSERT INTO operations(id,environment_id,kind,state,generation,created_at_ms,updated_at_ms,finished_at_ms,promoted) VALUES('operation-1','{id}','refresh','succeeded',1,1,2,2,1),('operation-2','{staging}','refresh','succeeded',1,3,4,4,1),('operation-3','{id}','refresh','failed',1,5,6,6,0);
         INSERT INTO deployments(id,environment_id,manifest_json,template_json,generation,created_at_ms) VALUES('operation-1','{id}','{manifest}','{template}',1,1),('operation-2','{staging}','{manifest}','{template}',1,3),('operation-3','{id}','{manifest}','{template}',1,5);"
    ))
    .execute(&pool)
    .await
    .unwrap();
    for (operation, environment) in [("operation-1", &production), ("operation-2", &staging)] {
        let target = target(&application, environment, &instance, pulled('a'));
        sqlx::query("UPDATE operations SET target_json=?1 WHERE id=?2")
            .bind(serde_json::to_string(&target).unwrap())
            .bind(operation)
            .execute(&pool)
            .await
            .unwrap();
    }
    pool.close().await;
    application.id().clone()
}

#[tokio::test]
async fn migration_records_releases_for_prepared_deployments() {
    let temp = tempfile::tempdir().unwrap();
    let application = schema_21_with_prepared_deployments(temp.path()).await;
    let store = Store::open(temp.path().join("db")).await.unwrap();

    let releases = store.releases(&application, None, 10).await.unwrap().items;
    let [release] = releases.as_slice() else {
        panic!("one shared release, got {releases:?}")
    };
    assert_eq!(
        release.release.sources()[&ServiceName::parse("web").unwrap()],
        pulled('a')
    );
    let (production, staging) = (
        EnvironmentId::default_for(&application),
        EnvironmentId::parse("env-staging-01").unwrap(),
    );
    assert_eq!(
        deployment_releases(&store, &production).await,
        BTreeMap::from([
            ("operation-1".to_owned(), Some(release.id.clone())),
            ("operation-3".to_owned(), None),
        ])
    );
    assert_eq!(
        deployment_releases(&store, &staging).await["operation-2"].as_ref(),
        Some(&release.id)
    );
    assert_eq!(
        store.current_release(&production).await.unwrap().as_ref(),
        Some(&release.id)
    );
    // Recording is idempotent across restarts.
    drop(store);
    let store = Store::open(temp.path().join("db")).await.unwrap();
    assert_eq!(
        store
            .releases(&application, None, 10)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
}
