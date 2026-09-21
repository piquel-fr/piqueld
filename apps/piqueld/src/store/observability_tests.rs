//! Behavioral tests for durable diagnostics, deletion, crash recovery and delivery policies.
use super::*;
use crate::api::{Mutation, MutationResponse};
use crate::config::{DaemonConfig, WebhookDestination, WebhookKind};
use piqueld_core::observability::{Diagnostic, EventFilter, EventScope};

async fn application(store: &Store) -> Operation {
    let manifest=piqueld_core::parse_toml("api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='observable'\n[spec]").unwrap();
    let (MutationResponse::Operation(op), _) = store
        .accept(Mutation::apply(manifest, None), Some(0), false, None)
        .await
        .unwrap()
    else {
        panic!("operation");
    };
    store
        .transition_operation(
            &op.operation_id,
            OperationState::Requested,
            OperationState::Running,
            None,
        )
        .await
        .unwrap();
    store.operation(&op.operation_id).await.unwrap()
}
fn notifications() -> DaemonConfig {
    let mut config = DaemonConfig::default();
    config.notifications.enabled = true;
    config.notifications.destinations.push(WebhookDestination {
        name: "test".into(),
        url: "https://example.com/private-token".into(),
        enabled: true,
        kind: WebhookKind::Json,
    });
    config
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
        "docker_unavailable",
        "Docker became unreachable".into(),
    );
    store
        .record_diagnostic(&diagnostic, None, Some(&op.application_id))
        .await
        .unwrap();
    let failure = Diagnostic::new(
        new_id("diagnostic"),
        "service_update_failed",
        "Service failed".into(),
    );
    store
        .record_diagnostic(&failure, None, Some(&op.application_id))
        .await
        .unwrap();
    let (MutationResponse::Operation(delete), _) = store
        .accept(
            Mutation::Delete {
                id: op.application_id.clone(),
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
    store
        .transition_operation(
            &delete.operation_id,
            OperationState::Requested,
            OperationState::Running,
            None,
        )
        .await
        .unwrap();
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
    assert_eq!(retained.application_id, Some(op.application_id));
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
    let config = notifications();
    let store = Store::open(path).await.unwrap().with_observability(&config);
    store.configure_deliveries().await.unwrap();
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
        store
            .transition_operation(
                &op.id,
                OperationState::Requested,
                OperationState::Running,
                None,
            )
            .await
            .unwrap();
    }
    assert_eq!(store.deliveries(None, 100).await.unwrap().items.len(), 1);
    let failed = store
        .filtered_events(
            &EventFilter {
                errors_only: true,
                ..EventFilter::default()
            },
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
    assert_eq!(deliveries.items[0].category, "recovery");
}
#[tokio::test]
async fn disabling_cancels_pending_and_reenabling_never_replays() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut config = notifications();
    let store = Store::open(&path)
        .await
        .unwrap()
        .with_observability(&config);
    store.configure_deliveries().await.unwrap();
    let op = application(&store).await;
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
    drop(store);
    config.notifications.enabled = false;
    let store = Store::open(&path)
        .await
        .unwrap()
        .with_observability(&config);
    store.configure_deliveries().await.unwrap();
    assert_eq!(
        store.deliveries(None, 100).await.unwrap().items[0].state,
        "cancelled"
    );
    drop(store);
    config.notifications.enabled = true;
    let store = Store::open(&path)
        .await
        .unwrap()
        .with_observability(&config);
    store.configure_deliveries().await.unwrap();
    store.process_notifications().await.unwrap();
    assert!(store.claim_delivery().await.unwrap().is_none());
    assert!(!format!("{config:?}").contains("private-token"));
    assert!(
        !serde_json::to_string(&config.view())
            .unwrap()
            .contains("private-token")
    );
}
#[tokio::test]
async fn sustained_observation_requires_continuity_and_emits_one_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let config = notifications();
    let store = Store::open(temp.path().join("db"))
        .await
        .unwrap()
        .with_observability(&config);
    store.configure_deliveries().await.unwrap();
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
async fn pruning_marks_stream_gaps_and_analytics_coverage() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
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
            None,
            1,
        )
        .await
        .unwrap()
        .items[0]
        .id;
    store.prune_events(now_ms() + 1).await.unwrap();
    assert!(matches!(
        store.check_event_resume(last - 1).await,
        Err(StoreError::HistoryExpired)
    ));
    let analytics = store.deployment_analytics(None, 0, now_ms()).await.unwrap();
    assert!(analytics.incomplete);
    assert_eq!(analytics.succeeded, 1);
}
