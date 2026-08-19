use super::coordinator::plan_requires_execution;
use super::*;
use crate::docker::SwarmState;
use crate::store::OperationRepository;
use crate::store::WorkState;
use piqueld_core::resource::{DesiredNetwork, DesiredService, DesiredVolume};
use piqueld_core::{ApplicationId, NormalizedApplication, ObservedApplication, parse_json};
use sqlx::{
    Connection, Executor,
    sqlite::{SqliteConnectOptions, SqliteConnection},
};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU32, Ordering},
};
use tokio::sync::Mutex;

struct FakeDocker {
    failures: AtomicU32,
    calls: AtomicU32,
    observed: Mutex<ObservedApplication>,
}
impl FakeDocker {
    fn new(failures: u32) -> Self {
        Self {
            failures: AtomicU32::new(failures),
            calls: AtomicU32::new(0),
            observed: Mutex::new(ObservedApplication::default()),
        }
    }
    async fn mutate(&self) -> Result<(), DockerError> {
        tokio::task::yield_now().await;
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                if v > 0 { Some(v - 1) } else { None }
            })
            .is_ok()
        {
            Err(DockerError::Request("fake mutation"))
        } else {
            Ok(())
        }
    }
}
#[async_trait]
impl DockerApi for FakeDocker {
    async fn ensure_swarm(&self, _: bool) -> Result<SwarmState, DockerError> {
        Ok(SwarmState::Ready)
    }
    async fn resolve_image(&self, _: &str) -> Result<String, DockerError> {
        Ok(format!(
            "docker.io/library/alpine@sha256:{}",
            "a".repeat(64)
        ))
    }
    async fn observe(&self, _: &ApplicationId) -> Result<ObservedApplication, DockerError> {
        Ok(self.observed.lock().await.clone())
    }
    async fn ensure_network(&self, _: &DesiredNetwork) -> Result<(), DockerError> {
        self.mutate().await
    }
    async fn ensure_volume(&self, _: &DesiredVolume) -> Result<(), DockerError> {
        self.mutate().await
    }
    async fn ensure_service(&self, _: &DesiredService) -> Result<(), DockerError> {
        self.mutate().await
    }
    async fn remove_service(
        &self,
        _: &str,
        _: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        self.mutate().await
    }
    async fn remove_network(
        &self,
        _: &str,
        _: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        self.mutate().await
    }
}
async fn handler(fake: Arc<FakeDocker>) -> (tempfile::TempDir, ReconcileHandler<FakeDocker>) {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteStore::open(temp.path().join("state.db"))
            .await
            .unwrap(),
    );
    let retry = RetryPolicy {
        attempts: 4,
        initial_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
        convergence_timeout: Duration::from_millis(20),
    };
    (
        temp,
        ReconcileHandler::new(fake, store).with_retry_policy(retry),
    )
}

fn application() -> NormalizedApplication {
    parse_json(r#"{"api_version":"piqueld.dev/v1alpha1","kind":"Application","metadata":{"name":"stale-generation"},"spec":{"services":[{"name":"web","source":{"type":"image","image":"example.test/web:1"},"replicas":1,"environment":{},"command":[],"arguments":[],"ports":[],"mounts":[],"secrets":[],"healthcheck":null,"resources":null}],"volumes":[],"routes":[]}}"#)
            .unwrap()
            .normalize(ApplicationId::parse("reconcile-stale").unwrap())
}

#[tokio::test]
async fn transient_docker_failures_retry_with_a_bound() {
    let fake = Arc::new(FakeDocker::new(2));
    let (_temp, handler) = handler(Arc::clone(&fake)).await;
    handler
        .retry(&CancellationToken::new(), || fake.mutate())
        .await
        .unwrap();
    assert_eq!(fake.calls.load(Ordering::SeqCst), 3);
}
#[tokio::test]
async fn cancellation_stops_retry_before_another_mutation() {
    let fake = Arc::new(FakeDocker::new(20));
    let (_temp, handler) = handler(Arc::clone(&fake)).await;
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert_eq!(
        handler
            .retry(&cancellation, || fake.mutate())
            .await
            .unwrap_err(),
        OperationError::Cancelled
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stale_generation_operations_do_not_touch_runtime() {
    let fake = Arc::new(FakeDocker::new(0));
    let (_temp, handler) = handler(Arc::clone(&fake)).await;
    let store = Arc::clone(&handler.store);
    let app = application();
    let stale = store
        .create(&app, None, &["stale operation".into()])
        .await
        .unwrap();
    let current = store.replace(&app, None, 1, &[]).await.unwrap();
    let scheduler = OperationScheduler::new(Arc::clone(&store), Arc::new(handler), 1, 1);

    scheduler
        .run_until_idle(CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(
        store.operation(&stale.operation_id).await.unwrap().state,
        WorkState::Succeeded
    );
    assert_eq!(
        store.operation_steps(&stale.operation_id).await.unwrap()[0].state,
        StepState::Skipped
    );
    assert_eq!(
        store.operation(&current.operation_id).await.unwrap().state,
        WorkState::Failed
    );
    assert!(
        store
            .pending_operations(MAX_PAGE_SIZE)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn superseded_recovery_with_a_failed_step_cancels_the_parent() {
    let fake = Arc::new(FakeDocker::new(0));
    let (_temp, handler) = handler(Arc::clone(&fake)).await;
    let store = Arc::clone(&handler.store);
    let app = application();
    let stale = store
        .create(&app, None, &["failed stale operation".into()])
        .await
        .unwrap();
    store
        .transition_operation(
            &stale.operation_id,
            WorkState::Pending,
            WorkState::Running,
            None,
        )
        .await
        .unwrap();
    let step = store
        .operation_steps(&stale.operation_id)
        .await
        .unwrap()
        .remove(0);
    store
        .transition_step(&step.id, StepState::Pending, StepState::Running, None)
        .await
        .unwrap();
    store
        .transition_step(
            &step.id,
            StepState::Running,
            StepState::Failed,
            Some(OperationError::DockerRequestFailed("test mutation")),
        )
        .await
        .unwrap();
    store
        .transition_operation(
            &stale.operation_id,
            WorkState::Running,
            WorkState::Recovery,
            None,
        )
        .await
        .unwrap();
    let current = store.replace(&app, None, 1, &[]).await.unwrap();
    let scheduler = OperationScheduler::new(Arc::clone(&store), Arc::new(handler), 1, 1);

    scheduler
        .run_until_idle(CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(
        store.operation(&stale.operation_id).await.unwrap().state,
        WorkState::Cancelled
    );
    assert_eq!(
        store.operation_steps(&stale.operation_id).await.unwrap()[0].state,
        StepState::Failed
    );
    assert_eq!(
        store.operation(&current.operation_id).await.unwrap().state,
        WorkState::Failed
    );
    assert!(
        store
            .pending_operations(MAX_PAGE_SIZE)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failures_before_a_step_runs_degrade_the_application() {
    let fake = Arc::new(FakeDocker::new(0));
    let (_temp, handler) = handler(fake).await;
    let store = Arc::clone(&handler.store);
    let app = application();
    let mutation = store
        .create(&app, None, &["unreachable step".into()])
        .await
        .unwrap();
    let scheduler = OperationScheduler::new(Arc::clone(&store), Arc::new(handler), 1, 1);

    scheduler
        .run_until_idle(CancellationToken::new())
        .await
        .unwrap();

    let operation = store.operation(&mutation.operation_id).await.unwrap();
    assert_eq!(operation.state, WorkState::Failed);
    assert_eq!(
        operation.error_code.as_deref(),
        Some(OperationError::ResolvedStateMissing.code())
    );
    let status = store.status(&app.id).await.unwrap();
    assert_eq!(status.state, ApplicationState::Degraded);
    assert_eq!(
        status.message.as_deref(),
        Some(OperationError::ResolvedStateMissing.message().as_str())
    );
}

#[tokio::test]
async fn stale_failures_do_not_degrade_the_current_generation() {
    let fake = Arc::new(FakeDocker::new(0));
    let (_temp, handler) = handler(fake).await;
    let app = application();
    let stale = handler
        .store
        .create(&app, None, &["stale operation".into()])
        .await
        .unwrap();
    handler.store.replace(&app, None, 1, &[]).await.unwrap();
    handler
        .store
        .set_status(
            &app.id,
            ApplicationState::Pending,
            ApplicationState::Deploying,
            None,
            None,
        )
        .await
        .unwrap();
    let operation = handler.store.operation(&stale.operation_id).await.unwrap();

    handler
        .record_operation_failure(&operation, OperationError::ResolvedStateMissing)
        .await
        .unwrap();

    assert_eq!(
        handler.store.status(&app.id).await.unwrap().state,
        ApplicationState::Deploying
    );
}

#[tokio::test]
async fn delete_with_missing_status_does_not_touch_runtime() {
    let fake = Arc::new(FakeDocker::new(0));
    let (temp, handler) = handler(Arc::clone(&fake)).await;
    let app = application();
    let resolved = compile_application(
        &app,
        InstanceId::parse(handler.store.instance_id().to_owned()).unwrap(),
        INGRESS_NETWORK,
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
    .unwrap();
    let network = resolved.networks.last().unwrap();
    let observed = ObservedApplication {
        networks: vec![piqueld_core::resource::ObservedNetwork {
            name: network.name.clone(),
            ingress: network.ingress,
            runtime_configuration_matches: true,
            labels: network.labels.clone(),
        }],
        ..Default::default()
    };
    *fake.observed.lock().await = observed.clone();
    handler
        .store
        .create(&app, Some(&resolved), &[])
        .await
        .unwrap();
    let request = PlanRequest::Delete {
        application_id: app.id.clone(),
        instance_id: resolved.instance_id.clone(),
    };
    let steps = plan(&request, &observed)
        .actions
        .iter()
        .map(PlanAction::operation_step)
        .collect::<Vec<_>>();
    let deletion = handler
        .store
        .request_delete(&app.id, 1, &steps)
        .await
        .unwrap();

    let options = SqliteConnectOptions::new()
        .filename(temp.path().join("state.db"))
        .create_if_missing(false);
    let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
    connection
        .execute(
            sqlx::query("DELETE FROM application_status WHERE application_id = ?")
                .bind(app.id.as_str()),
        )
        .await
        .unwrap();
    let operation = handler
        .store
        .operation(&deletion.operation_id)
        .await
        .unwrap();

    assert_eq!(
        handler
            .execute(&operation, &CancellationToken::new())
            .await
            .unwrap_err(),
        OperationError::JournalUnavailable
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn ownership_conflicts_are_never_retried() {
    let fake = Arc::new(FakeDocker::new(0));
    let (_temp, handler) = handler(Arc::clone(&fake)).await;
    let calls = AtomicU32::new(0);
    let error = handler
        .retry(&CancellationToken::new(), || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(DockerError::OwnershipConflict) }
        })
        .await
        .unwrap_err();
    assert_eq!(error, OperationError::OwnershipConflict);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn immutable_configuration_conflicts_are_never_retried() {
    let fake = Arc::new(FakeDocker::new(0));
    let (_temp, handler) = handler(Arc::clone(&fake)).await;
    let calls = AtomicU32::new(0);
    let error = handler
        .retry(&CancellationToken::new(), || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(DockerError::ConfigurationConflict) }
        })
        .await
        .unwrap_err();
    assert_eq!(error, OperationError::DockerConfigurationConflict);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancellation_interrupts_convergence_waits() {
    let fake = Arc::new(FakeDocker::new(0));
    let (_temp, handler) = handler(fake).await;
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert_eq!(
        handler
            .wait_service(
                &ApplicationId::parse("app-example").unwrap(),
                "missing",
                false,
                &cancellation,
            )
            .await
            .unwrap_err(),
        OperationError::Cancelled
    );
}

#[test]
fn retained_volume_only_plans_do_not_create_reconciliation_feedback() {
    let plan = piqueld_core::Plan {
        actions: vec![PlanAction {
            sequence: 1,
            kind: ActionKind::RetainVolume {
                name: "piqueld-app-example-data".into(),
            },
            reason: piqueld_core::planner::ActionReason::VolumeRetentionPolicy,
            risk: piqueld_core::planner::ActionRisk::None,
            mutates_runtime: false,
            destructive: false,
        }],
        ..Default::default()
    };
    assert!(!plan_requires_execution(&plan));
}
#[tokio::test]
async fn notify_coalesces_feedback_bursts() {
    let wake = Notify::new();
    wake.notify_one();
    wake.notify_one();
    tokio::time::timeout(Duration::from_millis(5), wake.notified())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(5), wake.notified())
            .await
            .is_err()
    );
}
