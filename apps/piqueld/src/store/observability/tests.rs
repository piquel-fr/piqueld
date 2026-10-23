//! Behavioral tests for durable diagnostics, deletion, crash recovery and delivery policies.
use crate::api::{Mutation, MutationResponse};
use crate::config::DaemonConfig;
use crate::store::testing::template;
use crate::store::*;
use piqueld_core::observability::{
    ApplicationSelection, DeliveryState, DestinationKind, DestinationName, DestinationRequest,
    DestinationSettings, Diagnostic, EventFilter, EventScope, NotificationCategory,
};

async fn application(store: &Store) -> Operation {
    named_application(store, "observable").await
}
/// Saves and deploys an empty application, returning its running operation.
async fn named_application(store: &Store, name: &str) -> Operation {
    let saved = store.save_template(template(name, "[spec]"), true).await;
    let operation_id = saved.operation_id.unwrap();
    store.mark_running(&operation_id).await;
    store.operation(&operation_id).await.unwrap()
}
/// Settings receiving every application, preview and category, except
/// security events, which destination changes raise.
fn settings() -> DestinationSettings {
    let mut settings = DestinationSettings {
        previews: true,
        ..DestinationSettings::new(DestinationKind::Json)
    };
    settings.categories.remove(&NotificationCategory::Security);
    settings
}
/// Adds the destination `test` with [`settings`].
async fn notify(store: &Store) {
    let url = "https://example.com/private-token";
    store.add_destination("test", url, settings()).await;
}
#[tokio::test]
async fn interrupted_actions_and_diagnostics_survive_restart_and_scope_controls_deletion() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let store = Store::open(&path).await.unwrap();
    let op = application(&store).await;
    let action = store
        .begin_action(Some(&op.id), "ensure_service", Some("web"))
        .await
        .unwrap();
    store.action_request(&action, 1).await.unwrap();
    drop(store);
    let store = Store::open(&path).await.unwrap();
    store.interrupt_actions(None).await.unwrap();
    let history = store
        .filtered_events(
            &EventFilter {
                action_id: Some(action.id.clone()),
                ..EventFilter::default()
            },
            &Visibility::ALL,
            None,
            100,
        )
        .await
        .unwrap();
    assert_eq!(
        history
            .items
            .iter()
            .map(|e| e.kind.as_str())
            .collect::<Vec<_>>(),
        [
            "action_started",
            "action_requested",
            "action_outcome_unknown"
        ]
    );
    let diagnostic = Diagnostic::new(
        new_id("diagnostic"),
        piqueld_core::observability::DiagnosticCode::DockerUnavailable,
        "Docker became unreachable".into(),
    );
    let failure = Diagnostic::new(
        new_id("diagnostic"),
        piqueld_core::observability::DiagnosticCode::ServiceUpdateFailed,
        "Service failed".into(),
    );
    for diagnostic in [&diagnostic, &failure] {
        let daemon = Attribution::default();
        let environment = Some(&op.environment_id);
        store
            .record_diagnostic(diagnostic, None, environment, daemon)
            .await
            .unwrap();
    }
    // Application history, including its environments', goes with the application.
    let (MutationResponse::Deleted(deleted), _) = store
        .accept(
            crate::api::Actor::Daemon,
            Mutation::DeleteApplication {
                id: ApplicationId::parse(op.environment_id.as_str()).unwrap(),
                environments: Vec::new(),
            },
            None,
            true,
            None,
        )
        .await
        .unwrap()
    else {
        panic!("delete");
    };
    let delete = &deleted.operations[0];
    store.mark_running(&delete.operation_id).await;
    store
        .finish_delete_operation(&store.operation(&delete.operation_id).await.unwrap())
        .await
        .unwrap();
    assert!(matches!(
        store.diagnostic(&failure.id).await,
        Err(StoreError::NotFound)
    ));
    let retained = store.diagnostic(&diagnostic.id).await.unwrap();
    assert_eq!(retained.scope, EventScope::Daemon);
    assert_eq!(retained.environment_id, Some(op.environment_id));
    assert!(
        store
            .events(None, None, 100)
            .await
            .unwrap()
            .items
            .iter()
            .all(|event| event.scope == EventScope::Daemon)
    );
}
#[tokio::test]
async fn retries_keep_cause_and_outbox_deduplicates_until_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let store = Store::open(path).await.unwrap();
    notify(&store).await;
    let mut op = application(&store).await;
    for _ in 0..2 {
        store
            .transition_operation(
                &op.id,
                OperationState::Running,
                OperationState::Failed,
                Some((
                    "service_update_failed",
                    "Update paused; inspect service logs",
                )),
            )
            .await
            .unwrap();
        store.process_notifications().await.unwrap();
        op = store.operation(&op.id).await.unwrap();
        store.retry_operation(&op).await.unwrap();
        store.mark_running(&op.id).await;
    }
    assert_eq!(store.deliveries(None, 100).await.unwrap().items.len(), 1);
    let failed = store
        .filtered_events(
            &EventFilter {
                errors_only: true,
                ..EventFilter::default()
            },
            &Visibility::ALL,
            None,
            100,
        )
        .await
        .unwrap();
    assert_eq!(failed.items.len(), 2);
    assert!(failed.items.iter().all(|event| event.diagnostic.is_some()));
    store
        .transition_operation(
            &op.id,
            OperationState::Running,
            OperationState::Succeeded,
            None,
        )
        .await
        .unwrap();
    store.process_notifications().await.unwrap();
    let deliveries = store.deliveries(None, 100).await.unwrap();
    assert_eq!(deliveries.items.len(), 2);
    assert_eq!(deliveries.items[0].category, NotificationCategory::Recovery);
    // Deliveries page like other listings, with opaque `v1:` cursors.
    let cursor = store
        .deliveries(None, 1)
        .await
        .unwrap()
        .next_cursor
        .unwrap();
    assert_eq!(cursor, format!("v1:{}", deliveries.items[0].id));
    let older = store.deliveries(Some(&cursor), 1).await.unwrap();
    assert_eq!(older.items[0].id, deliveries.items[1].id);
    let bare = store.deliveries(Some(&deliveries.items[0].id), 1).await;
    assert!(matches!(bare, Err(StoreError::InvalidInput)));
}

#[tokio::test]
async fn closed_incidents_cannot_be_retried_after_failed_delivery() {
    for failed_before_close in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("db")).await.unwrap();
        notify(&store).await;
        let op = application(&store).await;
        store
            .transition_operation(
                &op.id,
                OperationState::Running,
                OperationState::Failed,
                Some(("service_update_failed", "Service failed")),
            )
            .await
            .unwrap();
        store.process_notifications().await.unwrap();
        let (failure, _) = store.claim_delivery().await.unwrap().unwrap();
        let state = if failed_before_close {
            DeliveryState::Failed
        } else {
            DeliveryState::Pending
        };
        store
            .complete_delivery(&failure.id, state, Some("Receiver rejected alert"), 3600)
            .await
            .unwrap();
        if failed_before_close {
            // The same failure remains retryable while its condition is open.
            store
                .retry_delivery(crate::api::Actor::Daemon, &failure.id)
                .await
                .unwrap();
            store
                .complete_delivery(&failure.id, DeliveryState::Failed, Some("Rejected"), 0)
                .await
                .unwrap();
        }
        store
            .retry_operation(&store.operation(&op.id).await.unwrap())
            .await
            .unwrap();
        store.mark_running(&op.id).await;
        store
            .transition_operation(
                &op.id,
                OperationState::Running,
                OperationState::Succeeded,
                None,
            )
            .await
            .unwrap();
        store.process_notifications().await.unwrap();
        if !failed_before_close {
            store
                .complete_delivery(&failure.id, DeliveryState::Failed, Some("Rejected"), 0)
                .await
                .unwrap();
            assert!(store.claim_delivery().await.unwrap().is_none());
        }
        assert!(matches!(
            store
                .retry_delivery(crate::api::Actor::Daemon, &failure.id)
                .await,
            Err(StoreError::Retry(RetryError::Closed))
        ));
    }
}

#[tokio::test]
async fn failed_recovery_cannot_be_retried_after_its_incident_reopens() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    notify(&store).await;
    let op = application(&store).await;
    let rerun = async |to: OperationState, error: Option<(&str, &str)>| {
        store
            .transition_operation(&op.id, OperationState::Running, to, error)
            .await
            .unwrap();
        store.process_notifications().await.unwrap();
        let (delivery, _) = store.claim_delivery().await.unwrap().unwrap();
        store
            .retry_operation(&store.operation(&op.id).await.unwrap())
            .await
            .unwrap();
        store.mark_running(&op.id).await;
        delivery
    };
    let failure = rerun(
        OperationState::Failed,
        Some(("service_update_failed", "Service failed")),
    )
    .await;
    store
        .complete_delivery(&failure.id, DeliveryState::Delivered, None, 0)
        .await
        .unwrap();
    let recovery = rerun(OperationState::Succeeded, None).await;
    assert_eq!(recovery.category, NotificationCategory::Recovery);
    store
        .complete_delivery(&recovery.id, DeliveryState::Failed, Some("Rejected"), 0)
        .await
        .unwrap();
    // Before the incident reopens, the recovery is still accurate and retryable.
    store
        .retry_delivery(crate::api::Actor::Daemon, &recovery.id)
        .await
        .unwrap();
    store
        .complete_delivery(&recovery.id, DeliveryState::Failed, Some("Rejected"), 0)
        .await
        .unwrap();
    rerun(
        OperationState::Failed,
        Some(("service_update_failed", "Service failed")),
    )
    .await;
    assert!(matches!(
        store
            .retry_delivery(crate::api::Actor::Daemon, &recovery.id)
            .await,
        Err(StoreError::Retry(RetryError::Reopened))
    ));
}

/// Opens a daemon incident whose delivery to `test` fails, returning the
/// delivery's ID.
async fn failed_daemon_delivery(store: &Store) -> String {
    notify(store).await;
    let diagnostic = Diagnostic::new(
        new_id("diagnostic"),
        piqueld_core::observability::DiagnosticCode::InternalError,
        "Daemon failure".into(),
    );
    store
        .record_diagnostic(&diagnostic, None, None, Attribution::default())
        .await
        .unwrap();
    store.process_notifications().await.unwrap();
    let (delivery, _) = store.claim_delivery().await.unwrap().unwrap();
    store
        .complete_delivery(&delivery.id, DeliveryState::Failed, Some("Rejected"), 0)
        .await
        .unwrap();
    delivery.id
}
#[tokio::test]
async fn open_daemon_incidents_remain_manually_retryable() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    let delivery = failed_daemon_delivery(&store).await;
    store
        .retry_delivery(crate::api::Actor::Daemon, &delivery)
        .await
        .unwrap();
    assert_eq!(
        store.deliveries(None, 100).await.unwrap().items[0].state,
        DeliveryState::Pending
    );
}
/// Retry names why it refuses a delivery: one that has not failed, or whose
/// destination no longer receives it.
#[tokio::test]
async fn retry_refusals_name_their_reason() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    let delivery = failed_daemon_delivery(&store).await;
    let retry = || store.retry_delivery(crate::api::Actor::Daemon, &delivery);
    assert!(matches!(
        store
            .retry_delivery(crate::api::Actor::Daemon, "missing")
            .await,
        Err(StoreError::NotFound)
    ));
    retry().await.unwrap();
    assert!(matches!(
        retry().await,
        Err(StoreError::Retry(RetryError::NotFailed(
            DeliveryState::Pending
        )))
    ));
    store
        .complete_delivery(&delivery, DeliveryState::Failed, Some("Rejected"), 0)
        .await
        .unwrap();
    put(
        &store,
        &DestinationSettings {
            enabled: false,
            ..settings()
        },
        None,
    )
    .await;
    assert!(matches!(
        retry().await,
        Err(StoreError::Retry(RetryError::NotReceived))
    ));
}
/// As after a downgrade across a category a newer version added.
#[tokio::test]
async fn an_undeliverable_delivery_does_not_block_the_next() {
    let (_temp, store) = Store::temporary().await;
    notify(&store).await;
    for code in [
        piqueld_core::observability::DiagnosticCode::InternalError,
        piqueld_core::observability::DiagnosticCode::DnsRecordsFailed,
    ] {
        let diagnostic = Diagnostic::new(new_id("diagnostic"), code, "Daemon failure".into());
        store
            .record_diagnostic(&diagnostic, None, None, Attribution::default())
            .await
            .unwrap();
    }
    store.process_notifications().await.unwrap();
    // The delivery due first can no longer be decoded.
    let poisoned: String = sqlx::query_scalar(
        "UPDATE notification_deliveries SET category='retired' WHERE id=(SELECT id FROM \
         notification_deliveries ORDER BY next_attempt_ms,id LIMIT 1) RETURNING id",
    )
    .fetch_one(&store.pool)
    .await
    .unwrap();

    let (claimed, _) = store.claim_delivery().await.unwrap().unwrap();

    assert_ne!(claimed.id, poisoned);
    let settled: (String, Option<String>) =
        sqlx::query_as("SELECT state,last_error FROM notification_deliveries WHERE id=?1")
            .bind(&poisoned)
            .fetch_one(&store.pool)
            .await
            .unwrap();
    assert_eq!(
        settled,
        (
            "failed".into(),
            Some("Unknown notification category".into())
        )
    );
    // History leaves it out and it cannot be retried, rather than failing.
    let listed = store.deliveries(None, 100).await.unwrap().items;
    assert_eq!(
        listed.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
        [claimed.id.as_str()]
    );
    assert!(matches!(
        store
            .retry_delivery(crate::api::Actor::Daemon, &poisoned)
            .await,
        Err(StoreError::Retry(RetryError::UnknownCategory))
    ));
}
/// Replaces the settings of the destination `test`, keeping its URL unless
/// `url` is given.
async fn put(store: &Store, settings: &DestinationSettings, url: Option<&str>) {
    let request = DestinationRequest {
        settings: settings.clone(),
        url: url.map(Into::into),
    };
    store
        .put_notification_destination(
            Actor::Daemon,
            &DestinationName::parse("test").unwrap(),
            request,
        )
        .await
        .unwrap();
}
/// Delivers every due delivery, returning their destinations and categories.
async fn drain(store: &Store) -> Vec<(String, NotificationCategory)> {
    let mut delivered = Vec::new();
    while let Some((delivery, _)) = store.claim_delivery().await.unwrap() {
        store
            .complete_delivery(&delivery.id, DeliveryState::Delivered, None, 0)
            .await
            .unwrap();
        delivered.push((delivery.destination, delivery.category));
    }
    delivered
}
/// Fails `op`'s deployment and processes the failure's notifications.
async fn fail(store: &Store, op: &Operation) {
    store
        .transition_operation(
            &op.id,
            OperationState::Running,
            OperationState::Failed,
            Some(("service_update_failed", "Update paused")),
        )
        .await
        .unwrap();
    store.process_notifications().await.unwrap();
}
#[tokio::test]
async fn disabling_cancels_pending_and_reenabling_never_replays() {
    let (_temp, store) = Store::temporary().await;
    notify(&store).await;
    let op = application(&store).await;
    fail(&store, &op).await;
    let mut settings = settings();
    settings.enabled = false;
    put(&store, &settings, None).await;
    assert_eq!(
        store.deliveries(None, 100).await.unwrap().items[0].state,
        DeliveryState::Cancelled
    );
    settings.enabled = true;
    put(&store, &settings, None).await;
    store.process_notifications().await.unwrap();
    assert!(store.claim_delivery().await.unwrap().is_none());
}
/// A new URL makes the destination a new receiver, which is never sent the
/// recovery of a failure the old one received.
#[tokio::test]
async fn new_urls_receive_no_recovery_of_earlier_failures() {
    let (_temp, store) = Store::temporary().await;
    notify(&store).await;
    let op = application(&store).await;
    fail(&store, &op).await;
    let (failure, _) = store.claim_delivery().await.unwrap().unwrap();
    store
        .complete_delivery(&failure.id, DeliveryState::Delivered, None, 0)
        .await
        .unwrap();
    let settings = settings();
    put(&store, &settings, Some("https://example.com/private-token")).await;
    store.process_notifications().await.unwrap();
    assert!(
        store.claim_delivery().await.unwrap().is_none(),
        "unchanged URL"
    );
    put(&store, &settings, Some("https://example.com/new-token")).await;
    store
        .retry_operation(&store.operation(&op.id).await.unwrap())
        .await
        .unwrap();
    store.mark_running(&op.id).await;
    store
        .transition_operation(
            &op.id,
            OperationState::Running,
            OperationState::Succeeded,
            None,
        )
        .await
        .unwrap();
    store.process_notifications().await.unwrap();
    assert!(store.claim_delivery().await.unwrap().is_none());
    let history = store.deliveries(None, 100).await.unwrap().items;
    assert!(
        history
            .iter()
            .all(|d| d.category != NotificationCategory::Recovery)
    );
}
/// Enabling a destination again keeps the failures it was sent, so their
/// recovery still reaches it.
#[tokio::test]
async fn reenabled_destinations_still_receive_recoveries() {
    let (_temp, store) = Store::temporary().await;
    notify(&store).await;
    let op = application(&store).await;
    fail(&store, &op).await;
    let (failure, _) = store.claim_delivery().await.unwrap().unwrap();
    store
        .complete_delivery(&failure.id, DeliveryState::Delivered, None, 0)
        .await
        .unwrap();
    let mut settings = settings();
    settings.enabled = false;
    put(&store, &settings, None).await;
    settings.enabled = true;
    put(&store, &settings, None).await;
    store
        .retry_operation(&store.operation(&op.id).await.unwrap())
        .await
        .unwrap();
    store.mark_running(&op.id).await;
    store
        .transition_operation(
            &op.id,
            OperationState::Running,
            OperationState::Succeeded,
            None,
        )
        .await
        .unwrap();
    store.process_notifications().await.unwrap();
    let (recovery, _) = store.claim_delivery().await.unwrap().unwrap();
    assert_eq!(recovery.category, NotificationCategory::Recovery);
}
/// Destinations receive notifications about their applications, previews
/// only when they ask, and daemon notifications whatever their applications.
#[tokio::test]
async fn destinations_receive_their_applications_and_daemon_notifications() {
    let (_temp, store) = Store::temporary().await;
    notify(&store).await;
    let watched = named_application(&store, "watched").await;
    let other = named_application(&store, "other").await;
    let mut settings = settings();
    settings.previews = false;
    settings.applications = ApplicationSelection::Only(
        [ApplicationId::parse(watched.environment_id.as_str()).unwrap()].into(),
    );
    fail(&store, &other).await;
    // Narrowing the applications cancels what is pending about the others.
    put(&store, &settings, None).await;
    let cancelled = store.deliveries(None, 100).await.unwrap().items.remove(0);
    assert_eq!(cancelled.state, DeliveryState::Cancelled);
    assert!(store.claim_delivery().await.unwrap().is_none());
    fail(&store, &watched).await;
    store.record_daemon_started("0.1.0").await.unwrap();
    store.process_notifications().await.unwrap();
    let mut categories = Vec::new();
    while let Some((delivery, _)) = store.claim_delivery().await.unwrap() {
        categories.push(delivery.category);
        store
            .complete_delivery(&delivery.id, DeliveryState::Delivered, None, 0)
            .await
            .unwrap();
    }
    categories.sort();
    assert_eq!(
        categories,
        [
            NotificationCategory::DeploymentFailures,
            NotificationCategory::DaemonStarted
        ]
    );
}
#[tokio::test]
async fn sustained_observation_requires_continuity_and_emits_one_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    notify(&store).await;
    for timestamp in [1000, 31_000, 61_000, 91_000] {
        store
            .observe_condition("docker_unavailable", None, true, timestamp, 45_000)
            .await
            .unwrap();
    }
    assert!(store.deliveries(None, 100).await.unwrap().items.is_empty());
    store
        .observe_condition("docker_unavailable", None, true, 121_000, 45_000)
        .await
        .unwrap();
    assert_eq!(store.deliveries(None, 100).await.unwrap().items.len(), 1);
    store
        .observe_condition("docker_unavailable", None, false, 151_000, 45_000)
        .await
        .unwrap();
    assert_eq!(store.deliveries(None, 100).await.unwrap().items.len(), 2);
    for timestamp in [200_000, 400_000, 430_000] {
        store
            .observe_condition("docker_unavailable", None, true, timestamp, 45_000)
            .await
            .unwrap();
    }
    assert_eq!(store.deliveries(None, 100).await.unwrap().items.len(), 2);
}
#[tokio::test]
async fn open_incidents_survive_retention_until_recovery_is_delivered() {
    for daemon in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("db");
        let mut config = DaemonConfig::default();
        config.retention.daemon_event_days = 1;
        let store = Store::open(&path)
            .await
            .unwrap()
            .with_observability(&config);
        notify(&store).await;
        let op = application(&store).await;
        let application = (!daemon).then_some(op.environment_id.as_str());
        let key = application.unwrap_or("docker_unavailable");
        let started = now_ms() - 2 * 86_400_000;
        let cutoff = now_ms() - 86_400_000;
        store
            .observe_condition(key, application, true, started, 45_000)
            .await
            .unwrap();
        // Retention must also preserve an incident still waiting for its threshold.
        store.prune_events(cutoff).await.unwrap();
        store.prune_daemon_events().await.unwrap();
        for elapsed in [30_000, 60_000, 90_000, 120_000] {
            store
                .observe_condition(key, application, true, started + elapsed, 45_000)
                .await
                .unwrap();
        }
        let (failure, _) = store.claim_delivery().await.unwrap().unwrap();
        store
            .complete_delivery(&failure.id, DeliveryState::Delivered, None, 0)
            .await
            .unwrap();
        store.prune_events(cutoff).await.unwrap();
        store.prune_daemon_events().await.unwrap();
        assert!(store.event(failure.event_id).await.is_ok());
        drop(store);

        let store = Store::open(&path)
            .await
            .unwrap()
            .with_observability(&config);
        store.restart_failure_windows().await.unwrap();
        store
            .observe_condition(key, application, false, started + 150_000, 45_000)
            .await
            .unwrap();
        store.prune_events(cutoff).await.unwrap();
        store.prune_daemon_events().await.unwrap();
        let (recovery, _) = store.claim_delivery().await.unwrap().unwrap();
        assert_eq!(recovery.category, NotificationCategory::Recovery);
        assert_eq!(recovery.destination, failure.destination);
        assert!(store.event(failure.event_id).await.is_ok());
        store
            .complete_delivery(&recovery.id, DeliveryState::Delivered, None, 0)
            .await
            .unwrap();
        assert!(store.claim_delivery().await.unwrap().is_none());

        // Closing and acknowledging the incident releases its old history.
        store.prune_events(cutoff).await.unwrap();
        store.prune_daemon_events().await.unwrap();
        assert!(matches!(
            store.event(failure.event_id).await,
            Err(StoreError::NotFound)
        ));
        assert!(matches!(
            store.event(recovery.event_id).await,
            Err(StoreError::NotFound)
        ));
        assert!(store.deliveries(None, 100).await.unwrap().items.is_empty());
    }
}

#[tokio::test]
async fn pruning_marks_stream_gaps_and_analytics_coverage() {
    let (_temp, store) = Store::temporary().await;
    let op = application(&store).await;
    store
        .transition_operation(
            &op.id,
            OperationState::Running,
            OperationState::Succeeded,
            None,
        )
        .await
        .unwrap();
    let last = store
        .filtered_events(
            &EventFilter {
                descending: true,
                ..EventFilter::default()
            },
            &Visibility::ALL,
            None,
            1,
        )
        .await
        .unwrap()
        .items[0]
        .id;
    // A quiet filtered stream still advances to the newest scanned event.
    let quiet = EventFilter {
        kind: Some("no_such_kind".into()),
        ..EventFilter::default()
    };
    let (items, checkpoint) = store
        .stream_events(&quiet, &Visibility::ALL, 0, 10)
        .await
        .unwrap();
    assert!(items.is_empty());
    assert_eq!(checkpoint, last);
    store.prune_events(now_ms() + 1).await.unwrap();
    assert!(matches!(
        store
            .stream_events(&EventFilter::default(), &Visibility::ALL, last - 1, 1)
            .await,
        Err(StoreError::HistoryExpired)
    ));
    assert!(
        store
            .stream_events(&quiet, &Visibility::ALL, checkpoint, 10)
            .await
            .is_ok()
    );
    let analytics = store
        .deployment_analytics(None, &Visibility::ALL, 0, now_ms())
        .await
        .unwrap();
    assert!(analytics.incomplete);
    assert_eq!(analytics.succeeded, 1);
}

#[tokio::test]
async fn recovery_waits_for_its_destination_failure_across_restart_and_pruning() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let store = Store::open(&path).await.unwrap();
    notify(&store).await;
    let url = "https://example.com/second";
    store.add_destination("second", url, settings()).await;
    let op = application(&store).await;
    store
        .transition_operation(
            &op.id,
            OperationState::Running,
            OperationState::Failed,
            Some(("service_update_failed", "Service failed")),
        )
        .await
        .unwrap();
    store.process_notifications().await.unwrap();
    let (delayed, _) = store.claim_delivery().await.unwrap().unwrap();
    store
        .complete_delivery(
            &delayed.id,
            DeliveryState::Pending,
            Some("Receiver unavailable"),
            3600,
        )
        .await
        .unwrap();
    let (acknowledged, _) = store.claim_delivery().await.unwrap().unwrap();
    store
        .complete_delivery(&acknowledged.id, DeliveryState::Delivered, None, 0)
        .await
        .unwrap();
    drop(store);

    // A new destination did not receive the failure and must not receive recovery.
    let store = Store::open(&path).await.unwrap();
    let url = "https://example.com/new";
    store
        .add_destination("new-destination", url, settings())
        .await;
    store
        .retry_operation(&store.operation(&op.id).await.unwrap())
        .await
        .unwrap();
    store.mark_running(&op.id).await;
    store
        .transition_operation(
            &op.id,
            OperationState::Running,
            OperationState::Succeeded,
            None,
        )
        .await
        .unwrap();
    store.process_notifications().await.unwrap();
    let (recovery, _) = store.claim_delivery().await.unwrap().unwrap();
    assert_eq!(recovery.category, NotificationCategory::Recovery);
    assert_eq!(recovery.destination, acknowledged.destination);
    store
        .complete_delivery(&recovery.id, DeliveryState::Delivered, None, 0)
        .await
        .unwrap();
    assert!(store.claim_delivery().await.unwrap().is_none());
    assert_eq!(store.deliveries(None, 100).await.unwrap().items.len(), 4);
    drop(store);

    let store = Store::open(&path).await.unwrap();
    store.restart_failure_windows().await.unwrap();
    assert!(store.claim_delivery().await.unwrap().is_none());
    // The delayed failure's acknowledgement makes only its own recovery eligible.
    store
        .complete_delivery(&delayed.id, DeliveryState::Delivered, None, 0)
        .await
        .unwrap();
    store.prune_events(now_ms() + 1).await.unwrap();
    assert!(store.event(delayed.event_id).await.is_ok());
    let (recovery, _) = store.claim_delivery().await.unwrap().unwrap();
    assert_eq!(recovery.category, NotificationCategory::Recovery);
    assert_eq!(recovery.destination, delayed.destination);
    assert!(store.event(recovery.event_id).await.is_ok());
}

#[tokio::test]
async fn undelivered_failures_expire_and_cancel_recovery_even_when_nothing_is_due() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    notify(&store).await;
    let op = application(&store).await;
    store
        .transition_operation(
            &op.id,
            OperationState::Running,
            OperationState::Failed,
            Some(("service_update_failed", "Service failed")),
        )
        .await
        .unwrap();
    store.process_notifications().await.unwrap();
    let (failure, _) = store.claim_delivery().await.unwrap().unwrap();
    store
        .complete_delivery(
            &failure.id,
            DeliveryState::Pending,
            Some("Unavailable"),
            3600,
        )
        .await
        .unwrap();
    store
        .retry_operation(&store.operation(&op.id).await.unwrap())
        .await
        .unwrap();
    store.mark_running(&op.id).await;
    store
        .transition_operation(
            &op.id,
            OperationState::Running,
            OperationState::Succeeded,
            None,
        )
        .await
        .unwrap();
    store.process_notifications().await.unwrap();
    sqlx::query!(
        "UPDATE notification_deliveries SET retry_started_at_ms=0 WHERE id=?1",
        failure.id
    )
    .execute(&store.pool)
    .await
    .unwrap();
    assert!(store.claim_delivery().await.unwrap().is_none());
    let deliveries = store.deliveries(None, 100).await.unwrap().items;
    assert_eq!(deliveries.len(), 2);
    assert_eq!(
        deliveries
            .iter()
            .find(|d| d.id == failure.id)
            .unwrap()
            .state,
        DeliveryState::Failed
    );
    assert_eq!(
        deliveries
            .iter()
            .find(|d| d.category == NotificationCategory::Recovery)
            .unwrap()
            .state,
        DeliveryState::Cancelled
    );
}

#[tokio::test]
async fn recovery_waits_for_all_pending_failure_categories_at_the_same_destination() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    notify(&store).await;
    let op = application(&store).await;
    for code in ["git_build_failed", "service_update_failed"] {
        store
            .transition_operation(
                &op.id,
                OperationState::Running,
                OperationState::Failed,
                Some((code, "Attempt failed")),
            )
            .await
            .unwrap();
        store.process_notifications().await.unwrap();
        store
            .retry_operation(&store.operation(&op.id).await.unwrap())
            .await
            .unwrap();
        store.mark_running(&op.id).await;
    }
    let (first, _) = store.claim_delivery().await.unwrap().unwrap();
    store
        .complete_delivery(&first.id, DeliveryState::Delivered, None, 0)
        .await
        .unwrap();
    let (second, _) = store.claim_delivery().await.unwrap().unwrap();
    store
        .complete_delivery(
            &second.id,
            DeliveryState::Pending,
            Some("Unavailable"),
            3600,
        )
        .await
        .unwrap();
    store
        .transition_operation(
            &op.id,
            OperationState::Running,
            OperationState::Succeeded,
            None,
        )
        .await
        .unwrap();
    store.process_notifications().await.unwrap();
    assert!(store.claim_delivery().await.unwrap().is_none());
    // An unacknowledged terminal failure doesn't suppress recovery for an acknowledged one.
    store
        .complete_delivery(&second.id, DeliveryState::Failed, Some("Rejected"), 0)
        .await
        .unwrap();
    let (recovery, _) = store.claim_delivery().await.unwrap().unwrap();
    assert_eq!(recovery.category, NotificationCategory::Recovery);
    store
        .complete_delivery(&recovery.id, DeliveryState::Delivered, None, 0)
        .await
        .unwrap();
    assert!(matches!(
        store
            .retry_delivery(crate::api::Actor::Daemon, &second.id)
            .await,
        Err(StoreError::Retry(RetryError::Recovered))
    ));
    assert!(store.claim_delivery().await.unwrap().is_none());
}

/// History, the event stream, builds, and analytics only include the
/// applications a caller may read; daemon history needs its own visibility.
#[tokio::test]
async fn visibility_limits_history_builds_and_analytics() {
    use piqueld_core::access::Scope;
    let (_temp, store) = Store::temporary().await;
    let mut ids = Vec::new();
    for name in ["blog", "shop"] {
        let op = named_application(&store, name).await;
        store
            .transition_operation(
                &op.id,
                OperationState::Running,
                OperationState::Succeeded,
                None,
            )
            .await
            .unwrap();
        let source = piqueld_core::manifest::ValidatedSource::Image {
            image: "example/web:1".into(),
        };
        store
            .start_build(&op.environment_id, &op.id, "web", &source, None)
            .await
            .unwrap();
        ids.push(ApplicationId::parse(op.environment_id.as_str()).unwrap());
    }
    let daemon = Diagnostic::from_recorded_code(
        "diagnostic-daemon".into(),
        "docker_unavailable",
        "down".into(),
    );
    store
        .record_diagnostic(&daemon, None, None, Attribution::default())
        .await
        .unwrap();
    let blog = Scope::one(ids[0].clone());
    let only_blog = |event: &piqueld_core::Event| {
        event.scope == EventScope::Application && event.application_id.as_ref() == Some(&ids[0])
    };
    for daemon in [false, true] {
        let visible = Visibility {
            applications: blog.clone(),
            daemon,
        };
        let page = store
            .filtered_events(&EventFilter::default(), &visible, None, 100)
            .await
            .unwrap()
            .items;
        let (streamed, _) = store
            .stream_events(&EventFilter::default(), &visible, 0, 100)
            .await
            .unwrap();
        for events in [page, streamed] {
            assert!(events.iter().any(only_blog));
            assert!(
                events
                    .iter()
                    .all(|event| only_blog(event) || (daemon && event.scope == EventScope::Daemon))
            );
            assert_eq!(
                events.iter().any(|event| event.scope == EventScope::Daemon),
                daemon
            );
        }
    }
    let builds = store
        .builds(None, None, &blog, None, 50)
        .await
        .unwrap()
        .items;
    assert_eq!(builds.len(), 1);
    assert_eq!(builds[0].environment_id, ids[0].as_str());
    // Analytics also leave out hidden applications' events and, without
    // daemon visibility, daemon failures.
    let analytics = |applications: Scope, daemon: bool| {
        let store = &store;
        async move {
            let visible = Visibility {
                applications,
                daemon,
            };
            let analytics = store
                .deployment_analytics(None, &visible, 0, now_ms())
                .await
                .unwrap();
            let daemon_failure = analytics
                .failures
                .iter()
                .any(|failure| failure.code == "docker_unavailable");
            (analytics.succeeded, daemon_failure)
        }
    };
    assert_eq!(analytics(blog.clone(), false).await, (1, false));
    assert_eq!(analytics(Scope::All, false).await, (2, false));
    assert_eq!(analytics(Scope::All, true).await, (2, true));
}

/// A refused request from `peer`, as the audit middleware records it.
fn refused(peer: &str) -> NewAuditEvent {
    NewAuditEvent {
        action: "GET /api/v1/applications/{id}".into(),
        outcome: piqueld_core::audit::AuditOutcome::Denied,
        status: 404,
        user_id: None,
        username: None,
        credential_id: None,
        credential_kind: None,
        scoped: None,
        peer: Some(peer.into()),
        tailnet: None,
        request_id: None,
        application_id: None,
        environment_id: None,
        permission: None,
        operator: None,
    }
}

/// A refusal burst and a credential appearing on a new address each raise
/// one security event, delivered as a `security` notification; a first
/// address and further refusals within the burst cooldown do not. Failed
/// security deliveries can be retried.
#[tokio::test]
async fn refusal_bursts_and_new_addresses_notify_as_security() {
    let (_temp, store) = Store::temporary().await;
    let url = "https://example.com/private-token";
    let settings = DestinationSettings::new(DestinationKind::Json);
    store.add_destination("test", url, settings).await;
    // A sustained burst alerts once per cooldown, then again once it passes.
    for _ in 0..25 {
        store.record_audit(&refused("203.0.113.9")).await.unwrap();
    }
    let rearm =
        "UPDATE events SET created_at_ms=created_at_ms-660000 WHERE kind='access_denial_burst'";
    sqlx::query(rearm).execute(&store.pool).await.unwrap();
    store.record_audit(&refused("203.0.113.9")).await.unwrap();
    let user = piqueld_core::auth::User {
        id: "alice".into(),
        username: "alice".into(),
        display_name: String::new(),
    };
    store
        .seed_auth_user(&user, &piqueld_core::access::Grants::admin())
        .await;
    let credential = NewCredential {
        id: "laptop".into(),
        secret_hash: "hash".into(),
        kind: CredentialKind::Cli,
        name: "piquelctl",
        expires_at: None,
        grants: None,
    };
    store
        .insert_credential(&user.id, &credential)
        .await
        .unwrap();
    for address in ["192.0.2.1", "192.0.2.1", "198.51.100.7"] {
        let note = store.note_credential_address("laptop", "alice", address);
        note.await.unwrap();
    }
    store.process_notifications().await.unwrap();
    let deliveries = store.deliveries(None, 10).await.unwrap().items;
    // Security deliveries open no incident, yet stay manually retryable.
    let failed = &deliveries[0].id;
    let fail = "UPDATE notification_deliveries SET state='failed' WHERE id=?1";
    sqlx::query(fail)
        .bind(failed)
        .execute(&store.pool)
        .await
        .unwrap();
    store
        .retry_delivery(crate::api::Actor::Daemon, failed)
        .await
        .unwrap();
    let mut notified = Vec::new();
    for delivery in deliveries {
        assert_eq!(delivery.category, NotificationCategory::Security);
        let event = store.event(delivery.event_id).await.unwrap();
        notified.push(event.message.unwrap_or_default());
    }
    notified.sort();
    let burst = "20 requests refused within a minute from 203.0.113.9 as an anonymous caller";
    assert_eq!(
        notified,
        [
            burst,
            burst,
            "alice's cli \"piquelctl\" was used from a new address: 198.51.100.7",
        ]
    );
}

/// Each daemon start notifies as `daemon_started`, naming the previous
/// version only when it changed.
#[tokio::test]
async fn daemon_starts_notify_with_version_changes() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    notify(&store).await;
    for version in ["0.1.0", "0.1.0", "0.2.0"] {
        store.record_daemon_started(version).await.unwrap();
    }
    store.process_notifications().await.unwrap();
    let mut notified = Vec::new();
    for delivery in store.deliveries(None, 10).await.unwrap().items {
        assert_eq!(delivery.category, NotificationCategory::DaemonStarted);
        let event = store.event(delivery.event_id).await.unwrap();
        notified.push(event.message.unwrap_or_default());
    }
    notified.sort();
    assert_eq!(
        notified,
        [
            "piqueld 0.1.0 started",
            "piqueld 0.1.0 started",
            "piqueld 0.2.0 started (previously 0.1.0)",
        ]
    );
}

/// On the Unix socket, refusals by the host operator and by anonymous
/// callers share no account and no address, yet count separately.
#[tokio::test]
async fn operator_and_anonymous_refusals_burst_separately() {
    let (_temp, store) = Store::temporary().await;
    let socket = |uid: Option<u32>| {
        let mut event = refused("unused");
        event.peer = None;
        event.operator = uid.map(|uid| piqueld_core::auth::HostOperator { uid });
        event
    };
    let bursts = async || {
        let messages = "SELECT message FROM events WHERE kind='access_denial_burst' ORDER BY id";
        sqlx::query_scalar::<_, String>(messages)
            .fetch_all(&store.pool)
            .await
            .unwrap()
    };
    for uid in std::iter::repeat_n(None, 19).chain([Some(0)]) {
        store.record_audit(&socket(uid)).await.unwrap();
    }
    assert_eq!(bursts().await, Vec::<String>::new());
    // Each reaches the threshold on its own, and the cooldown is separate too.
    for uid in std::iter::once(None).chain(std::iter::repeat_n(Some(0), 19)) {
        store.record_audit(&socket(uid)).await.unwrap();
    }
    let messages = bursts().await;
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert!(messages[0].contains("an anonymous caller"), "{messages:?}");
    assert!(
        messages[1].contains("host operator (uid 0)"),
        "{messages:?}"
    );
}

/// Pruning stops at the oldest retained record: one recorded after the
/// clock stepped back keeps it, rather than taking it along.
#[tokio::test]
async fn audit_pruning_keeps_records_after_a_retained_one() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = Store::open(temp.path().join("db")).await.unwrap();
    store.audit_days = 1;
    for (peer, at) in [("a", 0), ("b", now_ms()), ("c", 0)] {
        store.record_audit_at(&refused(peer), at).await.unwrap();
    }
    store.prune_audit().await.unwrap();
    let kept = sqlx::query_scalar::<_, String>("SELECT peer FROM audit_events ORDER BY id")
        .fetch_all(&store.pool)
        .await
        .unwrap();
    assert_eq!(kept, ["b", "c"]);
    let verified = store.verify_audit().await.unwrap();
    assert_eq!((verified.checked, verified.broken_at), (2, None));
}

/// The audit chain detects edited, renumbered, inserted, and removed records,
/// and pruning keeps it verifiable without ever erasing a broken part.
#[tokio::test]
async fn the_audit_chain_detects_alteration_and_survives_pruning() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = Store::open(temp.path().join("db")).await.unwrap();
    store.audit_days = 1;
    let now = now_ms();
    for (peer, at) in [("a", 0), ("b", 0), ("c", now), ("d", now)] {
        let mut event = refused(peer);
        // Tailnet identities and the host operator, recorded only on some
        // requests, are linked too.
        event.tailnet = (peer == "d").then(|| "tag:ci on runner".into());
        event.operator = (peer == "d").then_some(piqueld_core::auth::HostOperator { uid: 1000 });
        store.record_audit_at(&event, at).await.unwrap();
    }
    let intact = store.verify_audit().await.unwrap();
    assert_eq!((intact.checked, intact.broken_at), (4, None));
    let renamed = "UPDATE audit_events SET tailnet='tag:prod on runner' WHERE peer='d'";
    sqlx::query(renamed).execute(&store.pool).await.unwrap();
    let d_id: i64 = sqlx::query_scalar("SELECT id FROM audit_events WHERE peer='d'")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert_eq!(store.verify_audit().await.unwrap().broken_at, Some(d_id));
    let restore = "UPDATE audit_events SET tailnet='tag:ci on runner' WHERE peer='d'";
    sqlx::query(restore).execute(&store.pool).await.unwrap();
    let operator = "UPDATE audit_events SET operator_uid=0 WHERE peer='d'";
    sqlx::query(operator).execute(&store.pool).await.unwrap();
    assert_eq!(store.verify_audit().await.unwrap().broken_at, Some(d_id));
    let restore = "UPDATE audit_events SET operator_uid=1000 WHERE peer='d'";
    sqlx::query(restore).execute(&store.pool).await.unwrap();
    let run = async |sql: &str| {
        sqlx::query(sql).execute(&store.pool).await.unwrap();
        store.verify_audit().await.unwrap()
    };
    // A broken part is never pruned away.
    let edited = run("UPDATE audit_events SET status=200 WHERE peer='b'").await;
    assert_eq!(edited.broken_at, Some(2));
    store.prune_audit().await.unwrap();
    assert_eq!(store.verify_audit().await.unwrap(), edited);
    run("UPDATE audit_events SET status=404 WHERE peer='b'").await;
    // Pruning the two oldest keeps the newest pruned one as the anchor.
    store.prune_audit().await.unwrap();
    let pruned = store.verify_audit().await.unwrap();
    assert_eq!((pruned.checked, pruned.broken_at), (2, None));
    assert_eq!(pruned.anchor.unwrap().id, 2);
    assert_eq!(pruned.head, intact.head);
    let edited = run("UPDATE audit_events SET environment_id='env-other' WHERE peer='c'").await;
    assert_eq!(edited.broken_at, Some(3));
    run("UPDATE audit_events SET environment_id=NULL WHERE peer='c'").await;
    let renumbered = run("UPDATE audit_events SET id=9223372036854775807 WHERE peer='d'").await;
    assert_eq!(renumbered.broken_at, Some(i64::MAX));
    // Nothing can follow the highest possible ID, rather than wrapping around.
    assert!(matches!(
        store.record_audit(&refused("e")).await,
        Err(StoreError::Corrupt)
    ));
    run("UPDATE audit_events SET id=4 WHERE peer='d'").await;
    for id in [-5, i64::MIN] {
        let forged = format!(
            "INSERT INTO audit_events(id,created_at_ms,action,outcome,status) \
            VALUES({id},0,'forged','allowed',200)"
        );
        assert_eq!(run(&forged).await.broken_at, Some(id));
        // Pruning keeps the forged record as evidence.
        store.prune_audit().await.unwrap();
        assert_eq!(store.verify_audit().await.unwrap().broken_at, Some(id));
        run(&format!("DELETE FROM audit_events WHERE id={id}")).await;
    }
    // The anchor is stored whole or not at all.
    let half = sqlx::query("UPDATE audit_chain SET pruned_link=NULL");
    assert!(half.execute(&store.pool).await.is_err());
    let removed = run("DELETE FROM audit_events WHERE peer='c'").await;
    assert_eq!(
        removed.broken_at,
        Some(4),
        "removing breaks the next record"
    );
}
/// Successful deployments notify only destinations receiving them, naming
/// the application and environment as of then.
#[tokio::test]
async fn successful_deployments_notify_with_their_names() {
    let (_temp, store) = Store::temporary().await;
    notify(&store).await;
    let mut deployments = settings();
    deployments.categories = [(
        NotificationCategory::DeploymentSucceeded,
        piqueld_core::observability::CategoryRule::default(),
    )]
    .into();
    let url = "https://example.com/deployments";
    store.add_destination("deployments", url, deployments).await;
    let op = named_application(&store, "shop").await;
    store
        .transition_operation(
            &op.id,
            OperationState::Running,
            OperationState::Succeeded,
            None,
        )
        .await
        .unwrap();
    store.process_notifications().await.unwrap();
    let (delivery, _) = store.claim_delivery().await.unwrap().unwrap();
    assert_eq!(delivery.destination, "deployments");
    assert_eq!(delivery.category, NotificationCategory::DeploymentSucceeded);
    assert_eq!(
        delivery.subject.describe().as_deref(),
        Some("shop / production")
    );
    store
        .complete_delivery(&delivery.id, DeliveryState::Delivered, None, 0)
        .await
        .unwrap();
    assert!(store.claim_delivery().await.unwrap().is_none());

    // After a failure, the success is the recovery alone.
    let mut both = settings();
    both.categories.retain(|category, _| {
        matches!(
            category,
            NotificationCategory::DeploymentFailures | NotificationCategory::Recovery
        )
    });
    both.categories.insert(
        NotificationCategory::DeploymentSucceeded,
        piqueld_core::observability::CategoryRule::default(),
    );
    let name = DestinationName::parse("deployments").unwrap();
    let request = DestinationRequest {
        settings: both,
        url: None,
    };
    store
        .put_notification_destination(Actor::Daemon, &name, request)
        .await
        .unwrap();
    store.retry_operation(&op).await.unwrap();
    store.mark_running(&op.id).await;
    fail(&store, &op).await;
    drain(&store).await;
    store
        .retry_operation(&store.operation(&op.id).await.unwrap())
        .await
        .unwrap();
    store.mark_running(&op.id).await;
    store
        .transition_operation(
            &op.id,
            OperationState::Running,
            OperationState::Succeeded,
            None,
        )
        .await
        .unwrap();
    store.process_notifications().await.unwrap();
    let mut delivered = drain(&store).await;
    delivered.sort();
    let recovery = NotificationCategory::Recovery;
    let expected = [
        ("deployments".to_owned(), recovery),
        ("test".to_owned(), recovery),
    ];
    assert_eq!(delivered, expected);
}
/// A deployment waits for the recovery it brought, and is sent when that
/// recovery is cancelled because the failure was never acknowledged; failed,
/// it can be retried like any occurrence.
#[tokio::test]
async fn deployments_follow_cancelled_recoveries_and_stay_retryable() {
    let (_temp, store) = Store::temporary().await;
    let mut both = settings();
    both.categories.insert(
        NotificationCategory::DeploymentSucceeded,
        piqueld_core::observability::CategoryRule::default(),
    );
    store
        .add_destination("test", "https://example.com/private-token", both)
        .await;
    let op = application(&store).await;
    fail(&store, &op).await;
    store
        .retry_operation(&store.operation(&op.id).await.unwrap())
        .await
        .unwrap();
    store.mark_running(&op.id).await;
    store
        .transition_operation(
            &op.id,
            OperationState::Running,
            OperationState::Succeeded,
            None,
        )
        .await
        .unwrap();
    store.process_notifications().await.unwrap();
    // The failure, still unacknowledged, holds the recovery, which holds the deployment.
    let (failure, _) = store.claim_delivery().await.unwrap().unwrap();
    assert_eq!(failure.category, NotificationCategory::DeploymentFailures);
    store
        .complete_delivery(
            &failure.id,
            DeliveryState::Pending,
            Some("Receiver down"),
            3600,
        )
        .await
        .unwrap();
    assert!(store.claim_delivery().await.unwrap().is_none());
    sqlx::query("UPDATE notification_deliveries SET retry_started_at_ms=0 WHERE id=?1")
        .bind(&failure.id)
        .execute(&store.pool)
        .await
        .unwrap();
    let (deployed, _) = store.claim_delivery().await.unwrap().unwrap();
    assert_eq!(deployed.category, NotificationCategory::DeploymentSucceeded);
    store
        .complete_delivery(&deployed.id, DeliveryState::Failed, Some("Rejected"), 0)
        .await
        .unwrap();
    store
        .retry_delivery(crate::api::Actor::Daemon, &deployed.id)
        .await
        .unwrap();
}
