//! Durable desired state and operation history.

use piqueld::store::{SqliteStore, StoreError};
use piqueld_core::resource::{ResolutionSet, ResolvedSource, compile_application};
use piqueld_core::{ApplicationId, InstanceId, OperationState, parse_toml};
use sqlx::{Connection, SqliteConnection};

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
async fn fresh_database_persists_resolved_state_and_deletion_intent() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("control-plane.db");
    let store = SqliteStore::open(&database)
        .await
        .expect("fresh database opens");
    let application = application();
    let desired = resolved(&application, store.instance_id());
    let created = store
        .save_application(&application, Some(&desired), None)
        .await
        .expect("application saved");
    let stored = store
        .get(&application.id)
        .await
        .expect("application readable");
    assert_eq!(stored.resolved, Some(desired.clone()));
    assert!(!stored.delete_intent);
    assert_eq!(
        store.operation(&created.id).await.unwrap().state,
        OperationState::Requested
    );

    let deleted = store
        .request_delete(&application.id, None)
        .await
        .expect("deletion saved");
    assert_eq!(
        store.operation(&created.id).await.unwrap().state,
        OperationState::Cancelled
    );
    assert_eq!(deleted.state, OperationState::Requested);
    assert!(store.get(&application.id).await.unwrap().delete_intent);
    drop(store);

    let reopened = SqliteStore::open(&database)
        .await
        .expect("database reopens");
    assert_eq!(
        reopened
            .get(&application.id)
            .await
            .unwrap()
            .resolved
            .unwrap(),
        desired
    );
    assert!(reopened.get(&application.id).await.unwrap().delete_intent);
    reopened
        .transition_operation(
            &deleted.id,
            OperationState::Requested,
            OperationState::Running,
            None,
        )
        .await
        .unwrap();
    let running = reopened.operation(&deleted.id).await.unwrap();
    reopened.finish_delete_operation(&running).await.unwrap();
    assert!(matches!(
        reopened.get(&application.id).await,
        Err(StoreError::NotFound)
    ));
    assert_eq!(
        reopened.operation(&deleted.id).await.unwrap().state,
        OperationState::Succeeded
    );
}

#[tokio::test]
async fn replacement_cancels_previous_work_and_retry_reuses_the_failed_operation() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let store = SqliteStore::open(directory.path().join("state.db"))
        .await
        .unwrap();
    let application = application();
    let first = store
        .save_application(
            &application,
            Some(&resolved(&application, store.instance_id())),
            None,
        )
        .await
        .unwrap();
    let mut replacement = application.clone();
    replacement.spec.services[0].replicas = 2;
    let replacement = replacement.normalize();
    let replaced = store
        .save_application(
            &replacement,
            Some(&resolved(&replacement, store.instance_id())),
            None,
        )
        .await
        .unwrap();
    assert_ne!(first.id, replaced.id);
    assert_eq!(
        store.operation(&first.id).await.unwrap().state,
        OperationState::Cancelled
    );
    assert_eq!(
        store.get(&application.id).await.unwrap().application,
        replacement
    );
    store
        .transition_operation(
            &replaced.id,
            OperationState::Requested,
            OperationState::Running,
            None,
        )
        .await
        .unwrap();
    store
        .transition_operation(
            &replaced.id,
            OperationState::Running,
            OperationState::Failed,
            Some(("docker_error", "unavailable")),
        )
        .await
        .unwrap();
    let failed = store.operation(&replaced.id).await.unwrap();
    let retried = store.retry_operation(&failed).await.unwrap();
    assert_eq!(retried.id, replaced.id);
    assert_eq!(retried.state, OperationState::Requested);
    assert_eq!(retried.error_code, None);
    assert_eq!(retried.error_message, None);
    assert!(matches!(
        store
            .get(&ApplicationId::parse("missing-app").unwrap())
            .await,
        Err(StoreError::NotFound)
    ));
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
        .save_application(
            &corrupt,
            Some(&resolved(&corrupt, store.instance_id())),
            None,
        )
        .await
        .expect("corrupt application is created");
    store
        .save_application(
            &healthy,
            Some(&resolved(&healthy, store.instance_id())),
            None,
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
        .save_application(&third, Some(&resolved(&third, store.instance_id())), None)
        .await
        .expect("third application is created");
    let fourth = application_named("app-persist-04", "wiki");
    store
        .save_application(&fourth, Some(&resolved(&fourth, store.instance_id())), None)
        .await
        .expect("fourth application is created");

    // The corrupt row consumes one slot, but must not hide later applications.
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
        vec![healthy.id.as_str()],
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
        vec![third.id.as_str(), fourth.id.as_str()],
        "the remaining healthy application follows the quarantined page"
    );
}

#[tokio::test]
async fn event_history_survives_operation_pruning_and_has_independent_retention() {
    let directory = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(directory.path().join("state.db"))
        .await
        .unwrap();
    let app = application();
    let desired = resolved(&app, store.instance_id());
    let first = store
        .save_application(&app, Some(&desired), Some(0))
        .await
        .unwrap();
    let second = store.request_refresh(&app.id, Some(1)).await.unwrap();
    store.prune_finished_operations(i64::MAX).await.unwrap();
    assert!(matches!(
        store.operation(&first.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(store.operation(&second.id).await.is_ok());
    let events = store.events(Some(&app.id), None, 100).await.unwrap();
    assert!(
        events
            .items
            .iter()
            .any(|event| event.operation_id.as_deref() == Some(&first.id)
                && event.kind == "operation_cancelled")
    );
    store.prune_events(i64::MAX).await.unwrap();
    assert!(
        store
            .events(None, None, 100)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert!(store.get(&app.id).await.is_ok());
    assert!(store.operation(&second.id).await.is_ok());
}
