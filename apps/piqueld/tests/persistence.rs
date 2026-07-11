#![allow(missing_docs)]

use libsql::Builder;
use piqueld::store::{
    ApplicationRepository, ApplicationState, BuildRepository, LibsqlStore, OperationRepository,
    StatusRepository, StepState, StoreError, WorkState,
};
use piqueld_core::{ApplicationId, NormalizedApplication, parse_json};
use serde_json::json;
use std::path::Path;

fn application(id: &str, name: &str, secret: Option<&str>) -> NormalizedApplication {
    let secrets = secret.map_or_else(String::new, |name| {
        format!(r#", "secrets":[{{"source":"{name}","target":null,"mode":"0400"}}]"#)
    });
    let source = format!(
        r#"{{
      "api_version":"piqueld.dev/v1alpha1","kind":"Application",
      "metadata":{{"name":"{name}"}},"spec":{{"services":[{{
        "name":"web","source":{{"type":"image","image":"example.test/web:1"}},
        "replicas":1,"environment":{{}},"command":[],"arguments":[],"ports":[],
        "mounts":[]{secrets},"healthcheck":null,"resources":null
      }}],"volumes":[],"routes":[]}}
    }}"#
    );
    parse_json(&source)
        .unwrap()
        .normalize(ApplicationId::parse(id).unwrap())
}

async fn raw(path: &Path, sql: &str) {
    let db = Builder::new_local(path).build().await.unwrap();
    db.connect().unwrap().execute_batch(sql).await.unwrap();
}

#[tokio::test]
async fn fresh_migration_instance_and_roundtrip_are_stable() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let store = LibsqlStore::open(&path).await.unwrap();
    let first_instance = store.instance_id().to_owned();
    let app = application("application-0001", "notes", None);
    let mutation = store
        .create(
            &app,
            Some(&json!({"image":"example.test/web@sha256:abc"})),
            &["resolve".into(), "deploy".into()],
        )
        .await
        .unwrap();
    let stored = store.get(&app.id).await.unwrap();
    assert_eq!(mutation.generation, 1);
    assert_eq!(stored.application, app);
    assert_eq!(stored.spec_hash, app.spec_hash());
    assert_eq!(
        stored.resolved.unwrap()["image"],
        "example.test/web@sha256:abc"
    );
    assert!(stored.created_at_ms <= stored.updated_at_ms);
    drop(store);
    assert_eq!(
        LibsqlStore::open(path).await.unwrap().instance_id(),
        first_instance
    );
}

#[tokio::test]
async fn missing_secret_rejects_the_whole_create_without_partial_rows() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let store = LibsqlStore::open(&path).await.unwrap();
    let app = application("application-0002", "secret-app", Some("database-url"));
    assert_eq!(
        store
            .create(&app, None, &["resolve".into()])
            .await
            .unwrap_err(),
        StoreError::MissingSecrets(vec!["database-url".into()])
    );
    assert_eq!(store.get(&app.id).await.unwrap_err(), StoreError::NotFound);
    store.declare_logical_secret("database-url").await.unwrap();
    store.create(&app, None, &["resolve".into()]).await.unwrap();
}

#[tokio::test]
async fn expected_generation_conflicts_and_delete_intent_are_atomic() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let store = LibsqlStore::open(&path).await.unwrap();
    let app = application("application-0003", "updates", None);
    store.create(&app, None, &["deploy".into()]).await.unwrap();
    store
        .replace(&app, None, 1, &["deploy".into()])
        .await
        .unwrap();
    assert_eq!(
        store.replace(&app, None, 1, &[]).await.unwrap_err(),
        StoreError::GenerationConflict {
            expected: 1,
            actual: 2
        }
    );
    let deletion = store
        .request_delete(
            &app.id,
            2,
            &["remove-services".into(), "retain-volumes".into()],
        )
        .await
        .unwrap();
    assert_eq!(deletion.generation, 3);
    let stored = store.get(&app.id).await.unwrap();
    assert!(stored.delete_intent);
    assert_eq!(stored.generation, 3);
    assert_eq!(
        store.status(&app.id).await.unwrap().state,
        ApplicationState::Deleting
    );
}

#[tokio::test]
async fn concurrent_writers_have_one_winner_and_one_clean_conflict() {
    let directory = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(
        LibsqlStore::open(directory.path().join("state.db"))
            .await
            .unwrap(),
    );
    let app = application("application-0007", "concurrent", None);
    store.create(&app, None, &[]).await.unwrap();
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let store = store.clone();
        let app = app.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            store.replace(&app, None, 1, &[]).await
        }));
    }
    barrier.wait().await;
    let results = [
        tasks.remove(0).await.unwrap(),
        tasks.remove(0).await.unwrap(),
    ];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(
                result,
                Err(StoreError::GenerationConflict {
                    expected: 1,
                    actual: 2
                })
            ))
            .count(),
        1
    );
    assert_eq!(store.get(&app.id).await.unwrap().generation, 2);
}

#[tokio::test]
async fn operation_insert_failure_rolls_back_delete_generation_and_status() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let store = LibsqlStore::open(&path).await.unwrap();
    let app = application("application-0004", "rollback", None);
    store.create(&app, None, &[]).await.unwrap();
    raw(&path, "CREATE TRIGGER reject_delete BEFORE INSERT ON operations WHEN NEW.kind='delete' BEGIN SELECT RAISE(ABORT, 'fixture secret-canary must not escape'); END;").await;
    assert_eq!(
        store.request_delete(&app.id, 1, &[]).await.unwrap_err(),
        StoreError::Database
    );
    let stored = store.get(&app.id).await.unwrap();
    assert_eq!(stored.generation, 1);
    assert!(!stored.delete_intent);
    assert_eq!(
        store.status(&app.id).await.unwrap().state,
        ApplicationState::Pending
    );
}

#[tokio::test]
async fn operation_insert_faults_roll_back_create_and_replacement() {
    let directory = tempfile::tempdir().unwrap();
    let create_path = directory.path().join("create.db");
    let create_store = LibsqlStore::open(&create_path).await.unwrap();
    raw(&create_path, "CREATE TRIGGER reject_create BEFORE INSERT ON operations WHEN NEW.kind='create' BEGIN SELECT RAISE(ABORT, 'injected'); END;").await;
    let create_app = application("application-0008", "create-fault", None);
    assert_eq!(
        create_store
            .create(&create_app, None, &[])
            .await
            .unwrap_err(),
        StoreError::Database
    );
    assert_eq!(
        create_store.get(&create_app.id).await.unwrap_err(),
        StoreError::NotFound
    );

    let replace_path = directory.path().join("replace.db");
    let replace_store = LibsqlStore::open(&replace_path).await.unwrap();
    let replace_app = application("application-0009", "replace-fault", None);
    replace_store.create(&replace_app, None, &[]).await.unwrap();
    raw(&replace_path, "CREATE TRIGGER reject_replace BEFORE INSERT ON operations WHEN NEW.kind='replace' BEGIN SELECT RAISE(ABORT, 'injected'); END;").await;
    assert_eq!(
        replace_store
            .replace(&replace_app, Some(&json!({"should":"rollback"})), 1, &[])
            .await
            .unwrap_err(),
        StoreError::Database
    );
    let unchanged = replace_store.get(&replace_app.id).await.unwrap();
    assert_eq!(unchanged.generation, 1);
    assert!(unchanged.resolved.is_none());
}

#[tokio::test]
async fn step_status_and_build_state_machines_reject_illegal_transitions_and_prune() {
    let directory = tempfile::tempdir().unwrap();
    let store = LibsqlStore::open(directory.path().join("state.db"))
        .await
        .unwrap();
    let app = application("application-0010", "states", None);
    let mutation = store.create(&app, None, &["resolve".into()]).await.unwrap();
    let step = store
        .operation_steps(&mutation.operation_id)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(
        store
            .transition_step(&step.id, StepState::Pending, StepState::Succeeded, None)
            .await
            .unwrap_err(),
        StoreError::IllegalTransition
    );
    store
        .transition_step(&step.id, StepState::Pending, StepState::Running, None)
        .await
        .unwrap();
    store
        .transition_step(&step.id, StepState::Running, StepState::Succeeded, None)
        .await
        .unwrap();
    assert_eq!(
        store
            .set_status(
                &app.id,
                ApplicationState::Pending,
                ApplicationState::Ready,
                Some(1),
                None
            )
            .await
            .unwrap_err(),
        StoreError::IllegalTransition
    );
    store
        .set_status(
            &app.id,
            ApplicationState::Pending,
            ApplicationState::Deploying,
            None,
            None,
        )
        .await
        .unwrap();
    store
        .set_status(
            &app.id,
            ApplicationState::Deploying,
            ApplicationState::Ready,
            Some(1),
            None,
        )
        .await
        .unwrap();

    let build = store
        .create_build(&mutation.operation_id, &app.id, "web")
        .await
        .unwrap();
    store
        .transition_build(&build.id, WorkState::Pending, WorkState::Running, None)
        .await
        .unwrap();
    store
        .transition_build(&build.id, WorkState::Running, WorkState::Succeeded, None)
        .await
        .unwrap();
    assert_eq!(
        store.build(&build.id).await.unwrap().state,
        WorkState::Succeeded
    );
    assert_eq!(store.prune_finished_before(i64::MAX, 1).await.unwrap(), 1);
    assert_eq!(
        store.build(&build.id).await.unwrap_err(),
        StoreError::NotFound
    );
}

#[tokio::test]
async fn interrupted_running_work_becomes_recovery_on_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let store = LibsqlStore::open(&path).await.unwrap();
    let app = application("application-0005", "recovery", None);
    let mutation = store.create(&app, None, &["resolve".into()]).await.unwrap();
    store
        .transition_operation(
            &mutation.operation_id,
            WorkState::Pending,
            WorkState::Running,
            None,
        )
        .await
        .unwrap();
    drop(store);
    let reopened = LibsqlStore::open(path).await.unwrap();
    assert_eq!(reopened.recover_interrupted().await.unwrap(), 1);
    assert_eq!(
        reopened
            .operation(&mutation.operation_id)
            .await
            .unwrap()
            .state,
        WorkState::Recovery
    );
}

#[tokio::test]
async fn newer_schema_and_corrupt_canonical_state_are_rejected_safely() {
    let directory = tempfile::tempdir().unwrap();
    let newer = directory.path().join("newer.db");
    raw(&newer, "PRAGMA user_version = 99;").await;
    assert!(matches!(
        LibsqlStore::open(newer).await,
        Err(StoreError::SchemaMismatch)
    ));

    let path = directory.path().join("corrupt.db");
    let store = LibsqlStore::open(&path).await.unwrap();
    let app = application("application-0006", "corrupt", None);
    store.create(&app, None, &[]).await.unwrap();
    raw(
        &path,
        &format!(
            "UPDATE applications SET desired_json='{{}}' WHERE id='{}'",
            app.id
        ),
    )
    .await;
    assert_eq!(store.get(&app.id).await.unwrap_err(), StoreError::Corrupt);
}

#[tokio::test]
async fn sanitized_errors_never_echo_database_or_secret_canaries() {
    let error = StoreError::Database;
    let public = error.public();
    for rendered in [
        error.to_string(),
        format!("{error:?}"),
        public.to_string(),
        format!("{public:?}"),
    ] {
        assert!(!rendered.contains("secret-canary"));
        assert!(!rendered.contains("SQLITE"));
    }
}

#[tokio::test]
async fn forward_upgrade_from_version_one_updates_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("v1.db");
    let db = Builder::new_local(&path).build().await.unwrap();
    let connection = db.connect().unwrap();
    connection
        .execute_batch(include_str!("../../../migrations/0001_control_plane.sql"))
        .await
        .unwrap();
    connection.execute("INSERT INTO instance_metadata(singleton,instance_id,schema_version,created_at_ms) VALUES(1,'instance-old',1,1)", ()).await.unwrap();
    connection
        .execute_batch("PRAGMA user_version = 1;")
        .await
        .unwrap();
    drop(connection);
    drop(db);
    assert_eq!(
        LibsqlStore::open(path).await.unwrap().instance_id(),
        "instance-old"
    );
}
