//! Fresh-database coverage for the supported durable lifecycle.

use piqueld::store::{ApplicationState, SCHEMA_VERSION, SqliteStore, StepState, WorkState};
use piqueld_core::resource::{ResolutionSet, ResolvedSource, compile_application};
use piqueld_core::{ApplicationId, InstanceId, parse_toml};
use sqlx::{Connection, SqliteConnection};
use std::path::Path;

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

fn application_named(id: &str, name: &str) -> piqueld_core::NormalizedApplication {
    let manifest =
        include_str!("../../../crates/piqueld-core/tests/fixtures/manifests/prebuilt.toml")
            .replacen("name = \"notes\"", &format!("name = \"{name}\""), 1);
    parse_toml(&manifest)
        .expect("fixture variant is valid")
        .normalize(ApplicationId::parse(id).expect("fixture ID is valid"))
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

    let (stored, status) = store
        .get_with_status(&application.id)
        .await
        .expect("application and status are readable");
    assert_eq!(stored.resolved, resolved);
    assert_eq!(stored.generation, 1);
    assert!(!stored.delete_intent);
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

    let replace_key = format!("sha256:{}", "c".repeat(64));
    let replace_request = format!("sha256:{}", "d".repeat(64));
    let replaced = store
        .replace_idempotent(
            &application,
            &resolved,
            1,
            &["ensure_network".into()],
            &replace_key,
            &replace_request,
        )
        .await
        .expect("replacement increments the generation");
    assert_eq!(replaced.generation, 2);
    assert_eq!(
        store
            .replace_idempotent(
                &application,
                &resolved,
                1,
                &["ensure_network".into()],
                &replace_key,
                &replace_request,
            )
            .await
            .expect("replacement retry is idempotent"),
        replaced
    );
    assert!(matches!(
        store
            .replace(&application, &resolved, 1, &["ensure_network".into()])
            .await,
        Err(piqueld::store::StoreError::GenerationConflict {
            expected: 1,
            actual: 2
        })
    ));
    let delete_key = format!("sha256:{}", "e".repeat(64));
    let delete_request = format!("sha256:{}", "f".repeat(64));
    let deleted = store
        .request_delete_idempotent(
            &application.id,
            2,
            &["remove_service".into()],
            &delete_key,
            &delete_request,
        )
        .await
        .expect("delete is idempotent");
    assert_eq!(
        store
            .request_delete_idempotent(
                &application.id,
                2,
                &["remove_service".into()],
                &delete_key,
                &delete_request,
            )
            .await
            .expect("delete retry is idempotent"),
        deleted
    );
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

    for state in ["failed", "cancelled"] {
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
        sqlx::query(
            "UPDATE operation_steps SET state=?1,error_code=NULL,error_message=NULL,attempt=2,started_at_ms=created_at_ms,finished_at_ms=created_at_ms,updated_at_ms=created_at_ms WHERE operation_id=?2",
        )
        .bind(state)
        .bind(&first.operation_id)
        .execute(&mut connection)
        .await
        .expect("steps can be marked terminal");
        drop(connection);
        let retry = store
            .request_delete(&application.id, 2, &["remove_network".into()])
            .await
            .expect("terminal delete is reset");
        assert_eq!(retry, first);
        let (operation, steps) = store
            .operation_with_steps(&first.operation_id)
            .await
            .expect("reset delete is readable");
        assert_eq!(operation.state, WorkState::Pending);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].position, 0);
        assert_eq!(steps[0].action, "remove_network");
        assert_eq!(steps[0].attempt, 0);
        assert_eq!(steps[0].state, StepState::Pending);
        assert_eq!(steps[0].error_code, None);
        assert_eq!(steps[0].error_message, None);
        assert_eq!(steps[0].started_at_ms, None);
        assert_eq!(steps[0].finished_at_ms, None);
    }
}

#[tokio::test]
async fn keyed_delete_replay_resurrects_cancelled_operations() {
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
    let key_hash = format!("sha256:{}", "1".repeat(64));
    let request_hash = format!("sha256:{}", "2".repeat(64));
    let deleted = store
        .request_delete_idempotent(
            &application.id,
            1,
            &["remove_service".into()],
            &key_hash,
            &request_hash,
        )
        .await
        .expect("keyed delete is durable");

    let mut connection =
        SqliteConnection::connect(&format!("sqlite://{}?mode=rwc", database.display()))
            .await
            .expect("database can be inspected");
    sqlx::query(
        "UPDATE operations SET state='cancelled',error_code=NULL,error_message=NULL,updated_at_ms=created_at_ms,finished_at_ms=created_at_ms WHERE id=?1",
    )
    .bind(&deleted.operation_id)
    .execute(&mut connection)
    .await
    .expect("operation can be marked cancelled");
    drop(connection);

    let resurrected = store
        .request_delete_idempotent(
            &application.id,
            1,
            &["remove_service".into()],
            &key_hash,
            &request_hash,
        )
        .await
        .expect("cancelled binding is resurrected");
    assert_eq!(resurrected, deleted);
    let (operation, steps) = store
        .operation_with_steps(&deleted.operation_id)
        .await
        .expect("resurrected delete is readable");
    assert_eq!(operation.state, WorkState::Pending);
    assert_eq!(steps[0].action, "remove_service");

    let live = store
        .request_delete_idempotent(
            &application.id,
            1,
            &["remove_service".into()],
            &key_hash,
            &request_hash,
        )
        .await
        .expect("resurrected binding replays live");
    assert_eq!(live, deleted);
}

#[tokio::test]
async fn keyed_replace_replay_after_failure_resets_the_failed_operation() {
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
    let key_hash = format!("sha256:{}", "3".repeat(64));
    let request_hash = application.spec_hash();
    let replaced = store
        .replace_idempotent(
            &application,
            &resolved,
            1,
            &["ensure_network".into()],
            &key_hash,
            &request_hash,
        )
        .await
        .expect("keyed replacement is durable");

    let mut connection =
        SqliteConnection::connect(&format!("sqlite://{}?mode=rwc", database.display()))
            .await
            .expect("database can be inspected");
    sqlx::query(
        "UPDATE operations SET state='failed',error_code='step_failed',error_message='step failed',updated_at_ms=created_at_ms,finished_at_ms=created_at_ms WHERE id=?1",
    )
    .bind(&replaced.operation_id)
    .execute(&mut connection)
    .await
    .expect("operation can be marked failed");
    drop(connection);

    let retried = store
        .replace_idempotent(
            &application,
            &resolved,
            2,
            &["ensure_network".into()],
            &key_hash,
            &request_hash,
        )
        .await
        .expect("dead replace binding is resurrected");
    assert_eq!(retried.generation, 2);
    assert_eq!(retried.operation_id, replaced.operation_id);
    let replay = store
        .replace_idempotent(
            &application,
            &resolved,
            2,
            &["ensure_network".into()],
            &key_hash,
            &request_hash,
        )
        .await
        .expect("resurrected binding replays");
    assert_eq!(replay, retried);
}

#[tokio::test]
async fn keyed_create_replay_after_success_returns_the_original_operation() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("control-plane.db");
    let store = SqliteStore::open(&database)
        .await
        .expect("fresh database opens");
    let application = application();
    let resolved = resolved(&application, store.instance_id());
    let key_hash = format!("sha256:{}", "5".repeat(64));
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
        .expect("keyed create is durable");
    mark_operation_succeeded(&database, &first.operation_id).await;

    let replay = store
        .create_idempotent(
            &application,
            &resolved,
            &["ensure_network".into()],
            &key_hash,
            &request_hash,
        )
        .await
        .expect("succeeded binding replays verbatim");
    assert_eq!(replay, first);
    let (operation, _) = store
        .operation_with_steps(&first.operation_id)
        .await
        .expect("original operation is readable");
    assert_eq!(operation.state, WorkState::Succeeded);
}

#[tokio::test]
async fn retention_prune_removes_only_expired_finished_history() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("control-plane.db");
    let store = SqliteStore::open(&database)
        .await
        .expect("fresh database opens");
    let expired = application();
    let fresh = application_named("app-persist-02", "archive");
    let expired_key = format!("sha256:{}", "6".repeat(64));
    let fresh_key = format!("sha256:{}", "7".repeat(64));
    let expired_created = store
        .create_idempotent(
            &expired,
            &resolved(&expired, store.instance_id()),
            &["ensure_network".into()],
            &expired_key,
            &expired.spec_hash(),
        )
        .await
        .expect("expired application is created");
    let fresh_created = store
        .create_idempotent(
            &fresh,
            &resolved(&fresh, store.instance_id()),
            &["ensure_network".into()],
            &fresh_key,
            &fresh.spec_hash(),
        )
        .await
        .expect("fresh application is created");
    mark_operation_succeeded(&database, &expired_created.operation_id).await;
    mark_operation_succeeded(&database, &fresh_created.operation_id).await;

    let mut connection =
        SqliteConnection::connect(&format!("sqlite://{}?mode=rwc", database.display()))
            .await
            .expect("database can be inspected");
    sqlx::query(
        "UPDATE operations SET created_at_ms=1,updated_at_ms=1,started_at_ms=1,finished_at_ms=1 WHERE id=?1",
    )
    .bind(&expired_created.operation_id)
    .execute(&mut connection)
    .await
    .expect("expired operation can be backdated");
    drop(connection);

    let counts = store
        .prune_finished_operations(1_000_000_000)
        .await
        .expect("pruning succeeds");
    assert_eq!(counts.operations, 1);
    assert_eq!(counts.idempotency_keys, 1);
    assert!(matches!(
        store
            .operation_with_steps(&expired_created.operation_id)
            .await,
        Err(piqueld::store::StoreError::NotFound)
    ));
    assert_eq!(
        store
            .create_idempotency(&expired.id, &expired_key, &expired.spec_hash())
            .await
            .expect("pruned binding lookup succeeds"),
        None
    );
    let (kept, kept_steps) = store
        .operation_with_steps(&fresh_created.operation_id)
        .await
        .expect("fresh operation survives pruning");
    assert_eq!(kept.state, WorkState::Succeeded);
    assert_eq!(kept_steps.len(), 1);
    assert!(
        store
            .create_idempotency(&fresh.id, &fresh_key, &fresh.spec_hash())
            .await
            .expect("surviving binding lookup succeeds")
            .is_some()
    );
}

#[tokio::test]
async fn reclaim_expired_running_returns_stale_operations_to_recovery() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("control-plane.db");
    let store = SqliteStore::open(&database)
        .await
        .expect("fresh database opens");
    let stale = application();
    let fresh = application_named("app-persist-02", "archive");
    let stale_created = store
        .create(
            &stale,
            &resolved(&stale, store.instance_id()),
            &["ensure_network".into()],
        )
        .await
        .expect("stale application is created");
    let fresh_created = store
        .create(
            &fresh,
            &resolved(&fresh, store.instance_id()),
            &["ensure_network".into()],
        )
        .await
        .expect("fresh application is created");

    let mut connection =
        SqliteConnection::connect(&format!("sqlite://{}?mode=rwc", database.display()))
            .await
            .expect("database can be inspected");
    sqlx::query(
        "UPDATE operations SET state='running',created_at_ms=1,updated_at_ms=1,started_at_ms=1 WHERE id=?1",
    )
    .bind(&stale_created.operation_id)
    .execute(&mut connection)
    .await
    .expect("stale operation can look expired");
    sqlx::query(
        "UPDATE operations SET state='running',started_at_ms=created_at_ms,updated_at_ms=created_at_ms+10000000 WHERE id=?1",
    )
    .bind(&fresh_created.operation_id)
    .execute(&mut connection)
    .await
    .expect("fresh operation can look current");
    drop(connection);

    let reclaimed = store
        .reclaim_expired_running(60_000)
        .await
        .expect("reclamation succeeds");
    assert_eq!(reclaimed, 1);
    let (stale_operation, _) = store
        .operation_with_steps(&stale_created.operation_id)
        .await
        .expect("stale operation is readable");
    assert_eq!(stale_operation.state, WorkState::Recovery);
    assert_eq!(stale_operation.started_at_ms, None);
    assert_eq!(stale_operation.finished_at_ms, None);
    let (fresh_operation, _) = store
        .operation_with_steps(&fresh_created.operation_id)
        .await
        .expect("fresh operation is readable");
    assert_eq!(fresh_operation.state, WorkState::Running);
    assert_eq!(
        fresh_operation.updated_at_ms,
        fresh_operation.created_at_ms + 10_000_000
    );
}

#[tokio::test]
async fn list_quarantines_corrupt_rows_and_get_stays_fail_closed() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("control-plane.db");
    let store = SqliteStore::open(&database)
        .await
        .expect("fresh database opens");
    let corrupt = application();
    let healthy = application_named("app-persist-02", "archive");
    store
        .create(
            &corrupt,
            &resolved(&corrupt, store.instance_id()),
            &["ensure_network".into()],
        )
        .await
        .expect("corrupt application is created");
    store
        .create(
            &healthy,
            &resolved(&healthy, store.instance_id()),
            &["ensure_network".into()],
        )
        .await
        .expect("healthy application is created");

    let mut connection =
        SqliteConnection::connect(&format!("sqlite://{}?mode=rwc", database.display()))
            .await
            .expect("database can be inspected");
    sqlx::query("UPDATE applications SET desired_json='{}' WHERE id=?1")
        .bind(corrupt.id.as_str())
        .execute(&mut connection)
        .await
        .expect("row can be corrupted");
    drop(connection);

    let page = store
        .list(None, 50)
        .await
        .expect("listing tolerates a corrupt row");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].application.id, healthy.id);
    assert_eq!(page.next_cursor, None);
    assert!(store.get(&corrupt.id).await.is_err());

    // A corrupt row inside a full page must not suppress the pagination
    // cursor: quarantined rows still consume page slots, so the surviving
    // applications stay reachable on later pages.
    let third = application_named("app-persist-03", "gallery");
    store
        .create(
            &third,
            &resolved(&third, store.instance_id()),
            &["ensure_network".into()],
        )
        .await
        .expect("third application is created");
    let fourth = application_named("app-persist-04", "wiki");
    store
        .create(
            &fourth,
            &resolved(&fourth, store.instance_id()),
            &["ensure_network".into()],
        )
        .await
        .expect("fourth application is created");

    // A full first page (limit + 1 fetched rows) holds the corrupt row plus
    // two healthy ones; without cursor accounting for quarantine, the fourth
    // application would be unreachable.
    let first_page = store
        .list(None, 2)
        .await
        .expect("first page tolerates a corrupt row");
    assert_eq!(
        first_page
            .items
            .iter()
            .map(|application| application.application.id.as_str())
            .collect::<Vec<_>>(),
        vec![healthy.id.as_str(), third.id.as_str()],
        "the corrupt row is quarantined inside the full page"
    );
    let next_cursor = first_page.next_cursor.expect("full page reports a cursor");

    let second_page = store
        .list(Some(next_cursor.as_str()), 2)
        .await
        .expect("second page tolerates a corrupt row");
    assert_eq!(
        second_page
            .items
            .iter()
            .map(|application| application.application.id.as_str())
            .collect::<Vec<_>>(),
        vec![fourth.id.as_str()],
        "the remaining healthy application follows the quarantined page"
    );
}
