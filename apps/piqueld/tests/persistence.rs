#![allow(missing_docs)]

use piqueld::store::{
    ApplicationRepository, ApplicationState, BuildRepository, OperationRepository, SqliteStore,
    StatusRepository, StepState, StoreError, WorkState,
};
use piqueld_core::{
    ApplicationId, InstanceId, NormalizedApplication, ResolutionSet, compile_application,
    parse_json,
    resource::{ResolvedApplication, ResolvedSource},
};
use sqlx::{
    Connection, Executor,
    sqlite::{SqliteConnectOptions, SqliteConnection},
};
use std::collections::BTreeMap;
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

fn resolved(app: &NormalizedApplication, instance_id: &str) -> ResolvedApplication {
    compile_application(
        app,
        InstanceId::parse(instance_id).unwrap(),
        "piqueld-ingress",
        &ResolutionSet {
            sources: BTreeMap::from([(
                "web".into(),
                ResolvedSource::Image {
                    requested: "example.test/web:1".into(),
                    digest_reference: format!("example.test/web@sha256:{}", "a".repeat(64)),
                },
            )]),
            secrets: BTreeMap::new(),
        },
    )
    .unwrap()
}

async fn raw(path: &Path, sql: &str) {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true);
    let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
    connection.execute(sqlx::raw_sql(sql)).await.unwrap();
}

#[tokio::test]
async fn fresh_migration_instance_and_roundtrip_are_stable() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let store = SqliteStore::open(&path).await.unwrap();
    let first_instance = store.instance_id().to_owned();
    let app = application("application-0001", "notes", None);
    let resolved = resolved(&app, store.instance_id());
    let mutation = store
        .create(&app, Some(&resolved), &["resolve".into(), "deploy".into()])
        .await
        .unwrap();
    let stored = store.get(&app.id).await.unwrap();
    assert_eq!(mutation.generation, 1);
    assert_eq!(stored.application, app);
    assert_eq!(stored.spec_hash, app.spec_hash());
    assert_eq!(stored.resolved.unwrap(), resolved);
    assert!(stored.created_at_ms <= stored.updated_at_ms);
    assert_eq!(store.list(None, 50).await.unwrap().items.len(), 1);
    assert_eq!(
        store.operations_for_application(&app.id, 10).await.unwrap()[0].id,
        mutation.operation_id
    );
    drop(store);
    assert_eq!(
        SqliteStore::open(path).await.unwrap().instance_id(),
        first_instance
    );
}

#[tokio::test]
async fn missing_secret_rejects_the_whole_create_without_partial_rows() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let store = SqliteStore::open(&path).await.unwrap();
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
    let store = SqliteStore::open(&path).await.unwrap();
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
        SqliteStore::open(directory.path().join("state.db"))
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
    let store = SqliteStore::open(&path).await.unwrap();
    let app = application("application-0004", "rollback", None);
    store.create(&app, None, &[]).await.unwrap();
    raw(&path, "CREATE TRIGGER reject_delete BEFORE INSERT ON operations WHEN NEW.kind='delete' BEGIN SELECT RAISE(ABORT, 'injected'); END;").await;
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
    let create_store = SqliteStore::open(&create_path).await.unwrap();
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
    let replace_store = SqliteStore::open(&replace_path).await.unwrap();
    let replace_app = application("application-0009", "replace-fault", None);
    let replacement_resolved = resolved(&replace_app, replace_store.instance_id());
    replace_store.create(&replace_app, None, &[]).await.unwrap();
    raw(&replace_path, "CREATE TRIGGER reject_replace BEFORE INSERT ON operations WHEN NEW.kind='replace' BEGIN SELECT RAISE(ABORT, 'injected'); END;").await;
    assert_eq!(
        replace_store
            .replace(&replace_app, Some(&replacement_resolved), 1, &[])
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
    let store = SqliteStore::open(directory.path().join("state.db"))
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
    assert_eq!(
        store
            .transition_step(&step.id, StepState::Pending, StepState::Running, None)
            .await
            .unwrap_err(),
        StoreError::IllegalTransition
    );
    store
        .transition_operation(
            &mutation.operation_id,
            WorkState::Pending,
            WorkState::Running,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .transition_operation(
                &mutation.operation_id,
                WorkState::Running,
                WorkState::Succeeded,
                None,
            )
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
    let completed_steps = store.operation_steps(&mutation.operation_id).await.unwrap();
    let completed_step = &completed_steps[0];
    assert_eq!(completed_step.attempt, 1);
    assert!(completed_step.finished_at_ms.is_some());
    store
        .transition_operation(
            &mutation.operation_id,
            WorkState::Running,
            WorkState::Succeeded,
            None,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn steps_are_claimed_in_order_and_only_one_can_run() {
    let directory = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(directory.path().join("ordered-steps.db"))
        .await
        .unwrap();
    let app = application("application-0021", "ordered-steps", None);
    let mutation = store
        .create(&app, None, &["first".into(), "second".into()])
        .await
        .unwrap();
    let steps = store.operation_steps(&mutation.operation_id).await.unwrap();
    store
        .transition_operation(
            &mutation.operation_id,
            WorkState::Pending,
            WorkState::Running,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .transition_step(&steps[1].id, StepState::Pending, StepState::Running, None,)
            .await
            .unwrap_err(),
        StoreError::IllegalTransition
    );
    store
        .transition_step(&steps[0].id, StepState::Pending, StepState::Running, None)
        .await
        .unwrap();
    assert_eq!(
        store
            .transition_step(&steps[1].id, StepState::Pending, StepState::Running, None,)
            .await
            .unwrap_err(),
        StoreError::IllegalTransition
    );
    // An idempotent running observation is not a new attempt.
    store
        .transition_step(&steps[0].id, StepState::Running, StepState::Running, None)
        .await
        .unwrap();
    assert_eq!(
        store.operation_steps(&mutation.operation_id).await.unwrap()[0].attempt,
        1
    );
}

#[tokio::test]
async fn terminal_operation_atomically_cancels_unfinished_children() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("terminal-tree.db");
    let store = SqliteStore::open(&path).await.unwrap();
    let app = application("application-0022", "terminal-tree", None);
    let mutation = store
        .create(&app, None, &["first".into(), "second".into()])
        .await
        .unwrap();
    store
        .transition_operation(
            &mutation.operation_id,
            WorkState::Pending,
            WorkState::Running,
            None,
        )
        .await
        .unwrap();
    let steps = store.operation_steps(&mutation.operation_id).await.unwrap();
    store
        .transition_step(&steps[0].id, StepState::Pending, StepState::Running, None)
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
    raw(&path, "CREATE TRIGGER reject_terminal_build BEFORE UPDATE ON builds WHEN NEW.state='cancelled' BEGIN SELECT RAISE(ABORT, 'injected'); END;").await;
    assert_eq!(
        store
            .transition_operation(
                &mutation.operation_id,
                WorkState::Running,
                WorkState::Failed,
                Some(("execution_failed", "operation execution failed")),
            )
            .await
            .unwrap_err(),
        StoreError::Database
    );
    assert_eq!(
        store.operation(&mutation.operation_id).await.unwrap().state,
        WorkState::Running
    );
    assert_eq!(
        store.build(&build.id).await.unwrap().state,
        WorkState::Running
    );
    raw(&path, "DROP TRIGGER reject_terminal_build;").await;
    store
        .transition_operation(
            &mutation.operation_id,
            WorkState::Running,
            WorkState::Failed,
            Some(("execution_failed", "operation execution failed")),
        )
        .await
        .unwrap();
    assert!(
        store
            .operation_steps(&mutation.operation_id)
            .await
            .unwrap()
            .iter()
            .all(|step| step.state == StepState::Cancelled && step.finished_at_ms.is_some())
    );
    assert_eq!(
        store.build(&build.id).await.unwrap().state,
        WorkState::Cancelled
    );
}

#[tokio::test]
async fn application_status_transitions_are_guarded_and_bounded() {
    let directory = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(directory.path().join("status.db"))
        .await
        .unwrap();
    let app = application("application-0020", "status", None);
    store.create(&app, None, &[]).await.unwrap();
    assert_eq!(
        store
            .set_status(
                &app.id,
                ApplicationState::Pending,
                ApplicationState::Ready,
                Some(1),
                None,
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
    assert_eq!(
        store
            .set_status(
                &app.id,
                ApplicationState::Ready,
                ApplicationState::Ready,
                None,
                None,
            )
            .await
            .unwrap_err(),
        StoreError::IllegalTransition
    );
    assert_eq!(
        store
            .set_status(
                &app.id,
                ApplicationState::Ready,
                ApplicationState::Ready,
                Some(1),
                Some(""),
            )
            .await
            .unwrap_err(),
        StoreError::InvalidInput
    );

    store.replace(&app, None, 1, &[]).await.unwrap();
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
            ApplicationState::Degraded,
            Some(2),
            Some("runtime is degraded"),
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .set_status(
                &app.id,
                ApplicationState::Degraded,
                ApplicationState::Degraded,
                Some(1),
                Some("stale observation"),
            )
            .await
            .unwrap_err(),
        StoreError::IllegalTransition
    );
}

#[tokio::test]
async fn build_outputs_are_durable_and_required_before_success() {
    let directory = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(directory.path().join("build.db"))
        .await
        .unwrap();
    let app = application("application-0018", "build-output", None);
    let mutation = store.create(&app, None, &[]).await.unwrap();
    let build = store
        .create_build(&mutation.operation_id, &app.id, "web")
        .await
        .unwrap();
    store
        .transition_operation(
            &mutation.operation_id,
            WorkState::Pending,
            WorkState::Running,
            None,
        )
        .await
        .unwrap();
    store
        .transition_build(&build.id, WorkState::Pending, WorkState::Running, None)
        .await
        .unwrap();
    assert_eq!(
        store
            .transition_build(&build.id, WorkState::Running, WorkState::Succeeded, None)
            .await
            .unwrap_err(),
        StoreError::IllegalTransition
    );
    store
        .record_build_output(
            &build.id,
            "0123456789abcdef",
            "registry.test/notes:web",
            &format!("registry.test/notes@sha256:{}", "b".repeat(64)),
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .record_build_output(
                &build.id,
                "different-commit",
                "registry.test/notes:web",
                &format!("registry.test/notes@sha256:{}", "b".repeat(64)),
            )
            .await
            .unwrap_err(),
        StoreError::IllegalTransition
    );
    store
        .transition_build(&build.id, WorkState::Running, WorkState::Succeeded, None)
        .await
        .unwrap();
    store
        .transition_operation(
            &mutation.operation_id,
            WorkState::Running,
            WorkState::Succeeded,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        store.build(&build.id).await.unwrap().state,
        WorkState::Succeeded
    );
    assert_eq!(
        store
            .builds_for_operation(&mutation.operation_id)
            .await
            .unwrap()
            .len(),
        1
    );
    let operation = store.operation(&mutation.operation_id).await.unwrap();
    assert!(operation.started_at_ms.is_some());
    assert!(operation.finished_at_ms.is_some());
    assert_eq!(store.prune_finished_before(i64::MAX, 1).await.unwrap(), 1);
    assert_eq!(
        store.build(&build.id).await.unwrap_err(),
        StoreError::NotFound
    );
    assert_eq!(
        store
            .prune_finished_operations_before(i64::MAX, 1)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store.operation(&mutation.operation_id).await.unwrap_err(),
        StoreError::NotFound
    );
}

#[tokio::test]
async fn interrupted_running_work_becomes_recovery_on_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let store = SqliteStore::open(&path).await.unwrap();
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
    let step = store
        .operation_steps(&mutation.operation_id)
        .await
        .unwrap()
        .remove(0);
    store
        .transition_step(&step.id, StepState::Pending, StepState::Running, None)
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
    drop(store);
    let reopened = SqliteStore::open(path).await.unwrap();
    assert_eq!(reopened.recover_interrupted().await.unwrap(), 3);
    assert_eq!(
        reopened
            .operation(&mutation.operation_id)
            .await
            .unwrap()
            .state,
        WorkState::Recovery
    );
    assert_eq!(
        reopened
            .operation_steps(&mutation.operation_id)
            .await
            .unwrap()[0]
            .state,
        StepState::Recovery
    );
    assert_eq!(
        reopened.build(&build.id).await.unwrap().state,
        WorkState::Recovery
    );
}

#[tokio::test]
async fn returning_an_operation_to_recovery_atomically_recovers_running_children() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("recovery-children.db");
    let store = SqliteStore::open(&path).await.unwrap();
    let app = application("application-0019", "recover-children", None);
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
    let step = store.operation_steps(&mutation.operation_id).await.unwrap()[0].clone();
    store
        .transition_step(&step.id, StepState::Pending, StepState::Running, None)
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
    raw(&path, "CREATE TRIGGER reject_child_recovery BEFORE UPDATE ON operation_steps WHEN NEW.state='recovery' BEGIN SELECT RAISE(ABORT, 'injected'); END;").await;
    assert_eq!(
        store
            .transition_operation(
                &mutation.operation_id,
                WorkState::Running,
                WorkState::Recovery,
                None,
            )
            .await
            .unwrap_err(),
        StoreError::Database
    );
    assert_eq!(
        store.operation(&mutation.operation_id).await.unwrap().state,
        WorkState::Running
    );
    raw(&path, "DROP TRIGGER reject_child_recovery;").await;
    store
        .transition_operation(
            &mutation.operation_id,
            WorkState::Running,
            WorkState::Recovery,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        store.build(&build.id).await.unwrap().state,
        WorkState::Recovery
    );
    assert_eq!(
        store.operation_steps(&mutation.operation_id).await.unwrap()[0].state,
        StepState::Recovery
    );
}

#[tokio::test]
async fn newer_schema_and_corrupt_canonical_state_are_rejected_safely() {
    let directory = tempfile::tempdir().unwrap();
    let newer = directory.path().join("newer.db");
    raw(&newer, "PRAGMA user_version = 99;").await;
    assert!(matches!(
        SqliteStore::open(newer).await,
        Err(StoreError::SchemaMismatch)
    ));

    let path = directory.path().join("corrupt.db");
    let store = SqliteStore::open(&path).await.unwrap();
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
async fn user_and_metadata_schema_versions_must_agree_before_upgrade() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mismatched-version.db");
    let store = SqliteStore::open(&path).await.unwrap();
    drop(store);
    raw(
        &path,
        "UPDATE instance_metadata SET schema_version=1 WHERE singleton=1;",
    )
    .await;
    assert!(matches!(
        SqliteStore::open(path).await,
        Err(StoreError::SchemaMismatch)
    ));
}

#[tokio::test]
async fn resolved_state_is_bound_to_desired_hash_instance_and_canonical_compilation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("resolved.db");
    let store = SqliteStore::open(&path).await.unwrap();
    let app = application("application-0011", "resolved", None);
    let wrong_instance = resolved(&app, "instance-wrong");
    assert_eq!(
        store
            .create(&app, Some(&wrong_instance), &[])
            .await
            .unwrap_err(),
        StoreError::Corrupt
    );
    assert_eq!(store.get(&app.id).await.unwrap_err(), StoreError::NotFound);

    let valid = resolved(&app, store.instance_id());
    store.create(&app, Some(&valid), &[]).await.unwrap();
    raw(
        &path,
        &format!(
            "UPDATE applications SET resolved_json=json_set(resolved_json,'$.name','tampered') WHERE id='{}'",
            app.id
        ),
    )
    .await;
    assert_eq!(store.get(&app.id).await.unwrap_err(), StoreError::Corrupt);
}

#[tokio::test]
async fn build_application_mismatches_are_rejected_before_persistence() {
    let directory = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(directory.path().join("foreign-keys.db"))
        .await
        .unwrap();
    let first = application("application-0012", "first", None);
    let second = application("application-0013", "second", None);
    let operation = store.create(&first, None, &[]).await.unwrap();
    store.create(&second, None, &[]).await.unwrap();
    assert_eq!(
        store
            .create_build(&operation.operation_id, &second.id, "web")
            .await
            .unwrap_err(),
        StoreError::InvalidInput
    );
}

#[tokio::test]
async fn delete_intent_cannot_be_repeated_or_silently_replaced() {
    let directory = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(directory.path().join("delete-state.db"))
        .await
        .unwrap();
    let app = application("application-0014", "delete-state", None);
    store.create(&app, None, &[]).await.unwrap();
    assert_eq!(
        store
            .set_status(
                &app.id,
                ApplicationState::Pending,
                ApplicationState::Deleting,
                None,
                None,
            )
            .await
            .unwrap_err(),
        StoreError::IllegalTransition
    );
    store.request_delete(&app.id, 1, &[]).await.unwrap();
    store
        .set_status(
            &app.id,
            ApplicationState::Deleting,
            ApplicationState::Failed,
            Some(2),
            Some("delete operation failed"),
        )
        .await
        .unwrap();
    assert_eq!(
        store.request_delete(&app.id, 2, &[]).await.unwrap_err(),
        StoreError::IllegalTransition
    );
    assert_eq!(
        store.replace(&app, None, 2, &[]).await.unwrap_err(),
        StoreError::IllegalTransition
    );
    assert_eq!(store.get(&app.id).await.unwrap().generation, 2);
}

#[tokio::test]
async fn transition_missing_rows_are_distinct_from_illegal_transitions() {
    let directory = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(directory.path().join("missing.db"))
        .await
        .unwrap();
    assert_eq!(
        store
            .transition_operation(
                "operation-missing",
                WorkState::Pending,
                WorkState::Running,
                None,
            )
            .await
            .unwrap_err(),
        StoreError::NotFound
    );
}

#[tokio::test]
async fn missing_status_rows_roll_back_replace_and_delete_mutations() {
    let directory = tempfile::tempdir().unwrap();
    for delete in [false, true] {
        let path = directory.path().join(format!("missing-status-{delete}.db"));
        let store = SqliteStore::open(&path).await.unwrap();
        let app = application(
            if delete {
                "application-0016"
            } else {
                "application-0015"
            },
            if delete {
                "delete-status"
            } else {
                "replace-status"
            },
            None,
        );
        store.create(&app, None, &[]).await.unwrap();
        raw(
            &path,
            &format!(
                "DELETE FROM application_status WHERE application_id='{}'",
                app.id
            ),
        )
        .await;
        let error = if delete {
            store.request_delete(&app.id, 1, &[]).await.unwrap_err()
        } else {
            store.replace(&app, None, 1, &[]).await.unwrap_err()
        };
        assert_eq!(error, StoreError::Corrupt);
        let stored = store.get(&app.id).await.unwrap();
        assert_eq!(stored.generation, 1);
        assert!(!stored.delete_intent);
    }
}

#[tokio::test]
async fn database_identity_columns_and_instance_id_are_revalidated() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("identity.db");
    let store = SqliteStore::open(&path).await.unwrap();
    let app = application("application-0017", "identity", None);
    store.create(&app, None, &[]).await.unwrap();
    raw(
        &path,
        &format!(
            "UPDATE applications SET name='tampered' WHERE id='{}'",
            app.id
        ),
    )
    .await;
    assert_eq!(store.get(&app.id).await.unwrap_err(), StoreError::Corrupt);
    drop(store);
    raw(
        &path,
        "PRAGMA ignore_check_constraints=ON; UPDATE instance_metadata SET instance_id='INVALID_ID' WHERE singleton=1; PRAGMA ignore_check_constraints=OFF;",
    )
    .await;
    assert!(matches!(
        SqliteStore::open(path).await,
        Err(StoreError::Corrupt)
    ));
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
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true);
    let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
    connection
        .execute(sqlx::raw_sql(include_str!(
            "../../../migrations/0001_control_plane.sql"
        )))
        .await
        .unwrap();
    connection
        .execute(sqlx::query("INSERT INTO instance_metadata(singleton,instance_id,schema_version,created_at_ms) VALUES(1,'instance-old',1,1)"))
        .await
        .unwrap();
    connection
        .execute(sqlx::query("PRAGMA user_version = 1;"))
        .await
        .unwrap();
    drop(connection);
    assert_eq!(
        SqliteStore::open(path).await.unwrap().instance_id(),
        "instance-old"
    );
}
