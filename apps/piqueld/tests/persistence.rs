//! Fresh-database coverage for the supported durable lifecycle.

use piqueld::store::{ApplicationState, SCHEMA_VERSION, SqliteStore, WorkState};
use piqueld_core::resource::{ResolutionSet, ResolvedSource, compile_application};
use piqueld_core::{ApplicationId, InstanceId, parse_toml};
use sqlx::{Connection, SqliteConnection};
use std::{os::unix::fs::PermissionsExt, path::Path};

/// Marks an operation and all of its steps as succeeded in the database.
///
/// # Examples
///
/// ```no_run
/// # async fn example(database: &std::path::Path, operation_id: &str) {
/// mark_operation_succeeded(database, operation_id).await;
/// # }
/// ```
async fn mark_operation_succeeded(database: &Path, operation_id: &str) {
    let mut connection =
        SqliteConnection::connect(&format!("sqlite://{}?mode=rwc", database.display()))
            .await
            .expect("database can be inspected");
    sqlx::query(
        "UPDATE operations SET state='running',started_at_ms=created_at_ms,updated_at_ms=created_at_ms WHERE id=?1",
    )
    .bind(operation_id)
    .execute(&mut connection)
    .await
    .expect("operation can be started");
    sqlx::query(
        "UPDATE operation_steps SET state='succeeded',started_at_ms=created_at_ms,finished_at_ms=created_at_ms,updated_at_ms=created_at_ms WHERE operation_id=?1",
    )
    .bind(operation_id)
    .execute(&mut connection)
    .await
    .expect("operation steps can be completed");
    sqlx::query(
        "UPDATE operations SET state='succeeded',finished_at_ms=created_at_ms,updated_at_ms=created_at_ms WHERE id=?1",
    )
    .bind(operation_id)
    .execute(&mut connection)
    .await
    .expect("operation can be completed");
}

fn application() -> piqueld_core::NormalizedApplication {
    parse_toml(include_str!(
        "../../../crates/piqueld-core/tests/fixtures/manifests/prebuilt.toml"
    ))
    .expect("fixture is valid")
    .normalize(ApplicationId::parse("app-persist-01").expect("fixture ID is valid"))
}

fn resolved(
    application: &piqueld_core::NormalizedApplication,
    instance_id: &str,
) -> piqueld_core::resource::ResolvedApplication {
    let resolutions = ResolutionSet {
        sources: [(
            "web".into(),
            ResolvedSource::Image {
                requested: "ghcr.io/example/notes:1.4.0".into(),
                digest_reference: format!("ghcr.io/example/notes@sha256:{}", "a".repeat(64)),
            },
        )]
        .into_iter()
        .collect(),
    };
    compile_application(
        application,
        InstanceId::parse(instance_id).expect("store instance ID is valid"),
        &resolutions,
    )
    .expect("fixture resolves")
}

#[tokio::test]
async fn fresh_database_persists_resolved_state_and_retains_volumes() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("control-plane.db");
    let store = SqliteStore::open(&database)
        .await
        .expect("fresh database opens");
    assert_eq!(SCHEMA_VERSION, 1);

    let application = application();
    let resolved = resolved(&application, store.instance_id());
    let created = store
        .create(&application, &resolved, &["ensure_network".into()])
        .await
        .expect("application is created");

    let stored = store
        .get(&application.id)
        .await
        .expect("application is readable");
    assert_eq!(stored.resolved, resolved);
    assert_eq!(stored.generation, 1);
    assert!(!stored.delete_intent);
    let status = store
        .status(&application.id)
        .await
        .expect("status is readable");
    assert_eq!(status.state, ApplicationState::Pending);

    let (operation, steps) = store
        .operation_with_steps(&created.operation_id)
        .await
        .expect("operation is readable");
    assert_eq!(operation.state, WorkState::Pending);
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].action, "ensure_network");

    let deleted = store
        .request_delete(&application.id, 1, &["remove_service".into()])
        .await
        .expect("delete intent is durable");
    assert_eq!(deleted.generation, 2);
    assert_eq!(
        store
            .status(&application.id)
            .await
            .expect("status is readable")
            .state,
        ApplicationState::Deleting
    );

    assert!(matches!(
        store
            .finalize_delete(&application.id, deleted.generation)
            .await,
        Err(piqueld::store::StoreError::IllegalTransition)
    ));
    mark_operation_succeeded(&database, &deleted.operation_id).await;
    store
        .finalize_delete(&application.id, deleted.generation)
        .await
        .expect("delete tombstone is durable after operation success");
    assert!(matches!(
        store.get(&application.id).await,
        Err(piqueld::store::StoreError::NotFound)
    ));
    assert!(
        store
            .list(None, 50)
            .await
            .expect("application list is readable")
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn generation_updates_and_create_idempotency_are_durable() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let store = SqliteStore::open(directory.path().join("control-plane.db"))
        .await
        .expect("fresh database opens");
    let application = application();
    let resolved = resolved(&application, store.instance_id());
    let key_hash = format!("sha256:{}", "b".repeat(64));
    let request_hash = application.spec_hash();
    let first = store
        .create_idempotent(
            &application,
            &resolved,
            &["ensure_network".into()],
            &key_hash,
            &request_hash,
        )
        .await
        .expect("idempotent create is durable");
    let replay = store
        .create_idempotent(
            &application,
            &resolved,
            &["ensure_network".into()],
            &key_hash,
            &request_hash,
        )
        .await
        .expect("idempotent retry is readable");
    assert_eq!(first, replay);

    let replaced = store
        .replace(&application, &resolved, 1, &["ensure_network".into()])
        .await
        .expect("replacement increments the generation");
    assert_eq!(replaced.generation, 2);
    assert!(matches!(
        store
            .replace(&application, &resolved, 1, &["ensure_network".into()])
            .await,
        Err(piqueld::store::StoreError::GenerationConflict {
            expected: 1,
            actual: 2
        })
    ));
}

#[tokio::test]
async fn missing_database_parent_is_created_without_rewriting_existing_parent_mode() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let parent = directory.path().join("nested").join("state");
    let database = parent.join("control-plane.db");
    SqliteStore::open(&database)
        .await
        .expect("missing database parents are created");
    assert!(parent.is_dir());

    let before = std::fs::symlink_metadata(&parent)
        .expect("created parent metadata")
        .permissions();
    SqliteStore::open(&database)
        .await
        .expect("existing database parent remains usable");
    let after = std::fs::symlink_metadata(&parent)
        .expect("existing parent metadata")
        .permissions();
    assert_eq!(before.mode(), after.mode());
}

#[tokio::test]
async fn symlinked_database_parent_is_rejected() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let target = directory.path().join("target");
    std::fs::create_dir(&target).expect("target directory is created");
    let link = directory.path().join("link");
    std::os::unix::fs::symlink(&target, &link).expect("parent symlink is created");

    let Err(error) = SqliteStore::open(link.join("control-plane.db")).await else {
        panic!("symlinked database parent was accepted");
    };
    assert!(matches!(error, piqueld::store::StoreError::PathSource(_)));
}

#[tokio::test]
async fn delete_requests_reuse_active_operations_and_reset_terminal_operations() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("control-plane.db");
    let store = SqliteStore::open(&database)
        .await
        .expect("fresh database opens");
    let application = application();
    let resolved = resolved(&application, store.instance_id());
    store
        .create(&application, &resolved, &["ensure_network".into()])
        .await
        .expect("application is created");
    let first = store
        .request_delete(&application.id, 1, &["remove_service".into()])
        .await
        .expect("delete is durable");

    for state in ["pending", "running", "recovery"] {
        let mut connection =
            SqliteConnection::connect(&format!("sqlite://{}?mode=rwc", database.display()))
                .await
                .expect("database can be inspected");
        if state == "running" {
            sqlx::query(
                "UPDATE operations SET state='running',started_at_ms=created_at_ms,updated_at_ms=created_at_ms WHERE id=?1",
            )
            .bind(&first.operation_id)
            .execute(&mut connection)
            .await
            .expect("operation can be marked running");
        } else {
            sqlx::query(
                "UPDATE operations SET state=?1,started_at_ms=NULL,finished_at_ms=NULL,updated_at_ms=created_at_ms WHERE id=?2",
            )
            .bind(state)
            .bind(&first.operation_id)
            .execute(&mut connection)
            .await
            .expect("operation can be marked queued");
        }
        let retry = store
            .request_delete(&application.id, 2, &["remove_service".into()])
            .await
            .expect("active delete is reused");
        assert_eq!(retry, first);
    }

    for (state, action) in [
        ("failed", "remove_network"),
        ("cancelled", "remove_service"),
    ] {
        let mut connection =
            SqliteConnection::connect(&format!("sqlite://{}?mode=rwc", database.display()))
                .await
                .expect("database can be inspected");
        sqlx::query(
            "UPDATE operations SET state=?1,error_code=NULL,error_message=NULL,updated_at_ms=created_at_ms,finished_at_ms=created_at_ms WHERE id=?2",
        )
        .bind(state)
        .bind(&first.operation_id)
        .execute(&mut connection)
        .await
        .expect("operation can be marked terminal");
        let retry = store
            .request_delete(&application.id, 2, &[action.into()])
            .await
            .expect("terminal delete is reset");
        assert_eq!(retry, first);
        let (operation, steps) = store
            .operation_with_steps(&first.operation_id)
            .await
            .expect("reset delete is readable");
        assert_eq!(operation.state, WorkState::Pending);
        assert_eq!(steps[0].action, action);
    }
}
