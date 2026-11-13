//! The controller's own guarantees: refusing foreign resources, bounded I/O, scans, traces and the action journal.

use super::*;

/// Captures traces written as JSON lines.
#[derive(Clone, Default)]
struct TraceCapture(Arc<parking_lot::Mutex<Vec<u8>>>);

impl std::io::Write for TraceCapture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn controller_converges_a_prebuilt_application_through_the_docker_seam() {
    let harness = ControllerHarness::new().await;
    let created = harness.create().await;
    harness.interrupt(&created.id).await;
    harness.scan().await;
    harness.assert_recovered(&created.id).await;

    harness.replace().await;
    harness.scan().await;
    assert_eq!(harness.replicas().await, 2);

    harness.scan().await;
    harness.repair_drift().await;
    let deletion = harness.delete().await;
    harness
        .docker
        .deny_network_removal
        .store(true, Ordering::Relaxed);
    harness.scan().await;
    let blocked = harness.operation(&deletion.id).await;
    assert_eq!(blocked.state, OperationState::Running);
    assert_eq!(blocked.error_code.as_deref(), Some("ownership_conflict"));
    assert!(blocked.finished_at_ms.is_none());
    assert!(
        harness
            .store
            .get(&harness.environment().await)
            .await
            .unwrap()
            .delete_intent()
    );
    harness
        .docker
        .deny_network_removal
        .store(false, Ordering::Relaxed);
    harness.scan().await;
    harness.assert_deleted(&deletion).await;
    assert!(matches!(
        harness.store.operation(&deletion.id).await,
        Err(piqueld_store::StoreError::NotFound)
    ));
}

#[tokio::test]
async fn controller_executes_actions_introduced_by_fresh_planning() {
    let harness = ControllerHarness::new().await;
    harness.seed_matching_runtime().await;

    harness.docker.observed.lock().await.networks.clear();
    harness.scan().await;

    let status = harness
        .store
        .status(&harness.environment().await)
        .await
        .unwrap();
    assert_eq!(status.state, piqueld_store::ApplicationState::Ready);
    assert_eq!(harness.docker.observed.lock().await.networks.len(), 1);
}

#[tokio::test]
async fn superseded_operations_do_not_plan_stale_runtime_state() {
    let harness = ControllerHarness::new().await;
    let stale = harness.seed_matching_runtime().await;

    harness.docker.observed.lock().await.networks.clear();
    let replacement = harness.replace().await;
    harness.scan().await;

    let stale_operation = harness.operation(&stale.id).await;
    assert_eq!(stale_operation.state, OperationState::Superseded);
    let replacement_operation = harness.operation(&replacement.id).await;
    assert_eq!(replacement_operation.state, OperationState::Succeeded);
}

/// A service, network or volume another installation created under the
/// name the application's would take is refused, never replaced.
#[tokio::test]
async fn controller_refuses_foreign_same_name_resources() {
    let foreign: [fn(&ResolvedApplication, &mut ObservedApplication); 3] = [
        |resolved, observed| {
            let mut labels = foreign_labels(&resolved.id);
            labels.insert(SERVICE_LABEL.into(), "web".into());
            observed.services.push(ObservedService {
                labels,
                ..observed_service(&resolved.services[0])
            });
        },
        |resolved, observed| {
            observed.networks.push(ObservedNetwork {
                name: resolved.networks[0].name.to_string(),
                runtime_configuration_matches: true,
                labels: foreign_labels(&resolved.id),
                in_use: false,
            });
        },
        |resolved, observed| {
            observed.volumes.push(ObservedVolume {
                name: resolved.volumes[0].name.to_string(),
                runtime_configuration_matches: true,
                labels: foreign_labels(&resolved.id),
            });
        },
    ];
    for add in foreign {
        ControllerHarness::new()
            .await
            .assert_refuses_foreign(add)
            .await;
    }
}

#[tokio::test]
async fn controller_enforces_global_io_bounds_on_a_single_thread() {
    let harness = ControllerHarness::new().await;
    for index in 0..12 {
        let mut manifest = manifest();
        manifest.metadata.name = format!("app-{index}");
        harness.apply(manifest, None).await;
    }
    harness.scan().await;
    let docker = &harness.docker;
    assert_eq!(docker.mutations.maximum.load(Ordering::SeqCst), 1);
    assert!((1..=2).contains(&docker.images.maximum.load(Ordering::SeqCst)));
    assert!((1..=8).contains(&docker.observations.maximum.load(Ordering::SeqCst)));
    for app in harness.store.list(None, 100).await.unwrap().items {
        assert_eq!(
            harness
                .store
                .latest_operation_for_environment(app.id())
                .await
                .unwrap()
                .unwrap()
                .state,
            OperationState::Succeeded
        );
    }
}

#[tokio::test]
async fn operation_traces_correlate_outcomes_without_configuration_values() {
    let capture = TraceCapture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || writer.clone())
        .finish();
    // Cargo runs these tests concurrently in one process. Install capture
    // globally and select this operation's events from the shared stream.
    tracing::subscriber::set_global_default(subscriber).unwrap();
    let (environment_id, operation_id) = {
        let mut harness = ControllerHarness::new().await;
        let mut manifest = harness.application.to_manifest();
        manifest.spec.services[0]
            .environment
            .insert("TOKEN".into(), "must-not-appear-in-traces".into());
        let application = manifest
            .validate()
            .unwrap()
            .normalize(harness.application.id().clone());
        harness.set_application(application);
        let operation = harness.create().await;
        harness.scan().await;
        (operation.environment_id.to_string(), operation.id)
    };
    let text = String::from_utf8(capture.0.lock().clone()).unwrap();
    assert!(!text.contains("must-not-appear-in-traces"));
    let events: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let completed = events
        .iter()
        .find(|event| {
            event["fields"]["message"] == "operation execution completed"
                && event["span"]["operation_id"] == operation_id
        })
        .unwrap_or_else(|| panic!("missing completion event in captured traces:\n{text}"));
    assert_eq!(completed["span"]["environment_id"], environment_id);
    assert_eq!(completed["span"]["operation_id"], operation_id);
    assert_eq!(completed["span"]["generation"], 1);
    assert_eq!(completed["fields"]["outcome"], "succeeded");
    assert!(completed["fields"]["duration_ms"].as_f64().is_some());
    assert!(events.iter().any(|event| {
        event["fields"]["message"] == "action completed"
            && event["spans"].as_array().is_some_and(|spans| {
                spans
                    .iter()
                    .any(|span| span["operation_id"] == operation_id)
            })
    }));
}

#[tokio::test]
async fn unavailable_action_journal_prevents_runtime_mutations() {
    let harness = ControllerHarness::new().await;
    harness.create().await;
    let mut connection = harness.database().await;
    sqlx::query("CREATE TRIGGER reject_action_journal BEFORE INSERT ON events WHEN NEW.kind='action_requested' BEGIN SELECT RAISE(FAIL,'journal unavailable'); END").execute(&mut connection).await.unwrap();
    assert!(
        harness
            .controller
            .scan(&CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(harness.docker.mutations.maximum.load(Ordering::SeqCst), 0);
    let open_actions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM active_actions")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert_eq!(open_actions, 0);
    let failed_actions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM events WHERE kind='action_failed' AND error_code='journal_unavailable'",
    )
    .fetch_one(&mut connection)
    .await
    .unwrap();
    assert!(failed_actions > 0);
    sqlx::query("DROP TRIGGER reject_action_journal")
        .execute(&mut connection)
        .await
        .unwrap();
    harness.scan().await;
    assert!(harness.docker.mutations.maximum.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn full_scan_records_one_docker_failure_for_overlapping_observers() {
    let harness = ControllerHarness::new().await;
    let operation = harness.create().await;
    harness.scan().await;
    assert_eq!(
        harness.operation(&operation.id).await.state,
        OperationState::Succeeded
    );
    let observed_before = harness.docker.observations.total.load(Ordering::SeqCst);
    harness
        .docker
        .fail_observations
        .store(true, Ordering::SeqCst);
    let cancellation = CancellationToken::new();
    let token = cancellation.clone();
    let controller = harness.controller;
    let task = tokio::spawn(async move {
        controller
            .run(
                Arc::new(tokio::sync::Notify::new()),
                Duration::from_mins(1),
                0,
                0,
                token,
            )
            .await
    });
    let recorded = async || {
        harness
            .store
            .filtered_events(
                &piqueld_core::observability::EventFilter {
                    environment_id: Some(operation.environment_id.to_string()),
                    kind: Some("diagnostic".into()),
                    error_code: Some("docker_unavailable".into()),
                    ..Default::default()
                },
                &piqueld_store::Visibility::ALL,
                None,
                10,
            )
            .await
            .unwrap()
            .items
            .len()
    };
    eventually(Duration::from_secs(5), POLL, async || {
        (harness.docker.observations.total.load(Ordering::SeqCst) >= observed_before + 2
            && recorded().await > 0)
            .then_some(())
    })
    .await
    .expect("health and reconciliation observers finished their failed Docker calls");
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancellation.cancel();
    task.await.unwrap().unwrap();
    assert_eq!(recorded().await, 1);
}

/// A scan processes the operation discovery found. One requested while the
/// scan retried cleanup is left to the next pass, rather than running under
/// the scan of the operation it replaced.
#[tokio::test]
async fn a_scan_leaves_operations_requested_after_discovery_to_the_next_pass() {
    let harness = ControllerHarness::new().await;
    harness.leave_job_cleanup_pending().await;
    let mut newer = None;
    harness
        .scan_during_cleanup(async {
            newer = Some(harness.apply(manifest(), None).await);
        })
        .await;
    let newer = newer.unwrap();
    assert_eq!(
        harness.operation(&newer.id).await.state,
        OperationState::Requested
    );
    harness.finish(&newer).await;
}

/// A scan acts on the current state of the operation discovery found. One
/// an earlier scan finished meanwhile is not run again, so its environment
/// isn't reported degraded by a failed journal write.
#[tokio::test]
async fn a_scan_leaves_an_operation_finished_since_discovery_alone() {
    let harness = ControllerHarness::new().await;
    harness.leave_job_cleanup_pending().await;
    let operation = harness.apply(manifest(), None).await;
    harness
        .store
        .transition_operation(
            &operation.id,
            OperationState::Requested,
            OperationState::Running,
            None,
        )
        .await
        .unwrap();
    let status = async || {
        harness
            .store
            .status(&operation.environment_id)
            .await
            .unwrap()
            .state
    };
    let before = status().await;
    harness
        .scan_during_cleanup(async {
            harness
                .store
                .transition_operation(
                    &operation.id,
                    OperationState::Running,
                    OperationState::Succeeded,
                    None,
                )
                .await
                .unwrap();
        })
        .await;
    assert_eq!(status().await, before);
    assert_eq!(
        harness.operation(&operation.id).await.state,
        OperationState::Succeeded
    );
}
