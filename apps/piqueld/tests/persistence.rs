//! Durable desired state and operation history.

#[path = "support/prebuilt.rs"]
mod prebuilt;
#[path = "support/sqlite.rs"]
mod sqlite;

use piqueld::store::{Store, StoreError};
use piqueld_core::api::Page;
use piqueld_core::manifest::ApplicationTemplate;
use piqueld_core::{EnvironmentId, NormalizedApplication, Operation, OperationState};

fn application() -> NormalizedApplication {
    prebuilt::application("app-persist-01")
}

/// The fixture saved as application `id`, named `name`.
fn application_named(id: &str, name: &str) -> NormalizedApplication {
    let mut manifest = prebuilt::manifest();
    manifest.metadata.name = name.into();
    manifest
        .validate()
        .expect("fixture variant is valid")
        .normalize(piqueld_core::ApplicationId::parse(id).expect("fixture ID is valid"))
}

/// The environment created with `application`, which shares its ID.
fn environment(application: &NormalizedApplication) -> EnvironmentId {
    EnvironmentId::default_for(application.id())
}

/// Saves `application` with its resolved target, without a revision check.
async fn save(store: &Store, application: &NormalizedApplication) -> Operation {
    store
        .save_application(
            &ApplicationTemplate::from(application),
            Some(&prebuilt::resolve(application, store.instance_id())),
            None,
        )
        .await
        .expect("application saved")
}

/// Every item of the pages `page` returns for each cursor, from the first.
async fn all_pages<T>(mut page: impl AsyncFnMut(Option<String>) -> Page<T>) -> Vec<T> {
    let mut items = Vec::new();
    let mut cursor = None;
    loop {
        let next = page(cursor).await;
        items.extend(next.items);
        cursor = next.next_cursor;
        if cursor.is_none() {
            return items;
        }
    }
}

#[tokio::test]
async fn fresh_database_persists_resolved_state_and_deletion_intent() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("control-plane.db");
    let store = Store::open(&database).await.expect("fresh database opens");
    let application = application();
    let environment = environment(&application);
    let created = save(&store, &application).await;
    let desired = prebuilt::resolve(&application, store.instance_id());
    let stored = store.get(&environment).await.expect("application readable");
    assert_eq!(stored.resolved, Some(desired.clone()));
    assert!(!stored.delete_intent());
    assert_eq!(
        store.operation(&created.id).await.unwrap().state,
        OperationState::Requested
    );

    let deleted = store
        .request_delete(&environment)
        .await
        .expect("deletion saved");
    assert_eq!(
        store.operation(&created.id).await.unwrap().state,
        OperationState::Superseded
    );
    assert_eq!(deleted.state, OperationState::Requested);
    assert!(store.get(&environment).await.unwrap().delete_intent());
    drop(store);

    let reopened = Store::open(&database).await.expect("database reopens");
    let stored = reopened.get(&environment).await.unwrap();
    assert!(stored.delete_intent());
    assert_eq!(stored.resolved.unwrap(), desired);
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
        reopened.get(&environment).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        reopened.operation(&deleted.id).await,
        Err(StoreError::NotFound)
    ));
    // Its events stay in the application's history until the application is deleted.
    let events = reopened
        .events(Some(&environment), None, 100)
        .await
        .unwrap();
    assert!(
        events
            .items
            .iter()
            .any(|event| event.application_id.as_ref() == Some(application.id()))
    );
}

#[tokio::test]
async fn replacement_cancels_previous_work_and_retry_reuses_the_failed_operation() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let store = Store::open(directory.path().join("state.db"))
        .await
        .unwrap();
    let application = application();
    let first = save(&store, &application).await;
    let mut replacement = application.to_manifest();
    replacement.spec.services[0].replicas = 2.into();
    let replacement = replacement
        .validate()
        .unwrap()
        .normalize(application.id().clone());
    let replaced = save(&store, &replacement).await;
    assert_ne!(first.id, replaced.id);
    assert_eq!(
        store.operation(&first.id).await.unwrap().state,
        OperationState::Superseded
    );
    assert_eq!(
        store
            .get(&environment(&application))
            .await
            .unwrap()
            .application
            .application,
        ApplicationTemplate::from(&replacement)
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
            .get(&EnvironmentId::parse("missing-app").unwrap())
            .await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn list_quarantines_corrupt_rows_and_get_stays_fail_closed() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("control-plane.db");
    let store = Store::open(&database).await.expect("fresh database opens");
    let corrupt = application();
    let healthy = application_named("app-persist-02", "archive");
    save(&store, &corrupt).await;
    save(&store, &healthy).await;

    sqlx::query("UPDATE applications SET desired_json='{}' WHERE id=?1")
        .bind(corrupt.id().as_str())
        .execute(&mut sqlite::connect(&database).await)
        .await
        .expect("row can be corrupted");

    let summaries = store
        .list_summaries(&piqueld_core::access::Scope::All, None, 50)
        .await
        .expect("summary listing does not read manifest documents");
    assert_eq!(summaries.items.len(), 2);
    assert_eq!(&summaries.items[0].id, corrupt.id());
    assert_eq!(&summaries.items[1].id, healthy.id());

    let page = store
        .list(None, 50)
        .await
        .expect("listing tolerates a corrupt row");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id(), &environment(&healthy));
    assert_eq!(page.next_cursor, None);
    assert!(store.get(&environment(&corrupt)).await.is_err());

    // A corrupt row inside a full page must not suppress the pagination
    // cursor: quarantined rows still consume page slots, so the surviving
    // applications stay reachable on later pages.
    let third = application_named("app-persist-03", "gallery");
    save(&store, &third).await;
    let fourth = application_named("app-persist-04", "wiki");
    save(&store, &fourth).await;

    // The corrupt row consumes one slot, but must not hide later applications.
    let ids = |page: &piqueld::store::EnvironmentPage| {
        page.items
            .iter()
            .map(|environment| environment.id().to_string())
            .collect::<Vec<_>>()
    };
    let first_page = store
        .list(None, 2)
        .await
        .expect("first page tolerates a corrupt row");
    assert_eq!(
        ids(&first_page),
        [healthy.id().as_str()],
        "the corrupt row is quarantined inside the full page"
    );
    let next_cursor = first_page.next_cursor.expect("full page reports a cursor");

    let second_page = store
        .list(Some(next_cursor.as_str()), 2)
        .await
        .expect("second page tolerates a corrupt row");
    assert_eq!(
        ids(&second_page),
        [third.id().as_str(), fourth.id().as_str()],
        "the remaining healthy application follows the quarantined page"
    );
}

#[tokio::test]
async fn deployment_history_survives_pruning_and_events_have_independent_retention() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("state.db"))
        .await
        .unwrap();
    let app = application();
    let desired = prebuilt::resolve(&app, store.instance_id());
    let first = store
        .save_application(&ApplicationTemplate::from(&app), Some(&desired), Some(0))
        .await
        .unwrap();
    let second = store
        .request_deploy(&environment(&app), Some(1))
        .await
        .unwrap();
    store.prune_finished_operations(i64::MAX).await.unwrap();
    assert!(store.operation(&first.id).await.is_ok());
    assert!(store.operation(&second.id).await.is_ok());
    assert!(store.prepared_target(&first.id).await.unwrap().is_some());
    assert!(store.prepared_target(&second.id).await.unwrap().is_none());
    let events = store
        .events(Some(&environment(&app)), None, 100)
        .await
        .unwrap();
    assert!(
        events
            .items
            .iter()
            .any(|event| event.operation_id.as_deref() == Some(&first.id)
                && event.kind == "operation_superseded")
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
    assert!(store.get(&environment(&app)).await.is_ok());
    assert!(store.operation(&second.id).await.is_ok());
}

#[tokio::test]
async fn history_pages_remain_application_scoped_and_follow_id_cursors() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("history.db");
    let store = Store::open(&database).await.unwrap();
    let app = application();
    let environment = environment(&app);
    let other = application_named("app-other", "other");
    let initial = store
        .save_application(&ApplicationTemplate::from(&app), None, Some(0))
        .await
        .unwrap();
    store
        .save_application(&ApplicationTemplate::from(&other), None, Some(0))
        .await
        .unwrap();
    let mut expected_deployments = vec![initial.id];
    for _ in 0..3 {
        expected_deployments.push(
            store
                .request_deploy(&environment, Some(1))
                .await
                .unwrap()
                .id,
        );
        store
            .request_deploy(&EnvironmentId::default_for(other.id()), Some(1))
            .await
            .unwrap();
    }
    expected_deployments.sort_unstable_by(|left, right| right.cmp(left));

    // Equal creation times must not change the API's ID-based order or cursor.
    sqlx::query("UPDATE deployments SET created_at_ms=1")
        .execute(&mut sqlite::connect(&database).await)
        .await
        .unwrap();
    let deployments = all_pages(async |cursor| {
        store
            .deployments(&environment, cursor.as_deref(), 2)
            .await
            .unwrap()
    })
    .await;
    for deployment in &deployments {
        assert_eq!(deployment.operation.environment_id, environment);
    }
    let deployments = deployments
        .into_iter()
        .map(|deployment| deployment.operation.id)
        .collect::<Vec<_>>();
    assert_eq!(deployments, expected_deployments);

    let expected_events = store
        .events(None, None, 100)
        .await
        .unwrap()
        .items
        .into_iter()
        .filter(|event| event.environment_id.as_ref() == Some(&environment))
        .map(|event| event.id)
        .collect::<Vec<_>>();
    let events = all_pages(async |cursor| {
        store
            .events(Some(&environment), cursor.as_deref(), 2)
            .await
            .unwrap()
    })
    .await;
    for event in &events {
        assert_eq!(event.environment_id.as_ref(), Some(&environment));
    }
    let events = events.into_iter().map(|event| event.id).collect::<Vec<_>>();
    assert_eq!(events, expected_events);
    let absent = EnvironmentId::parse("app-absent").unwrap();
    assert!(
        store
            .events(Some(&absent), None, 2)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}
