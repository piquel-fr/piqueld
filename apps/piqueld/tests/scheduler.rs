#![allow(missing_docs)]

use async_trait::async_trait;
use piqueld::{
    operations::{OperationHandler, OperationScheduler},
    store::{
        ApplicationRepository, Operation, OperationKind, OperationRepository, SqliteStore,
        WorkState,
    },
};
use piqueld_core::{ApplicationId, NormalizedApplication, parse_json};
use sqlx::{
    Connection,
    sqlite::{SqliteConnectOptions, SqliteConnection},
};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

fn application(id: &str, name: &str) -> NormalizedApplication {
    parse_json(&format!(r#"{{"api_version":"piqueld.dev/v1alpha1","kind":"Application","metadata":{{"name":"{name}"}},"spec":{{"services":[{{"name":"web","source":{{"type":"image","image":"example.test/web:1"}},"replicas":1,"environment":{{}},"command":[],"arguments":[],"ports":[],"mounts":[],"secrets":[],"healthcheck":null,"resources":null}}],"volumes":[],"routes":[]}}}}"#)).unwrap().normalize(ApplicationId::parse(id).unwrap())
}

struct ClaimedStateHandler(AtomicBool);

#[async_trait]
impl OperationHandler for ClaimedStateHandler {
    async fn execute(
        &self,
        operation: &Operation,
        _: &CancellationToken,
    ) -> Result<(), (&'static str, &'static str)> {
        self.0.store(
            operation.state == WorkState::Running && operation.started_at_ms.is_some(),
            Ordering::SeqCst,
        );
        Ok(())
    }
}

#[tokio::test]
async fn handlers_receive_the_durably_claimed_operation() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteStore::open(directory.path().join("claimed-state.db"))
            .await
            .unwrap(),
    );
    let app = application("scheduler-app-07", "claimed-state");
    store.create(&app, None, &[]).await.unwrap();
    let handler = Arc::new(ClaimedStateHandler(AtomicBool::new(false)));
    OperationScheduler::new(store, handler.clone(), 1, 1)
        .run_until_idle(CancellationToken::new())
        .await
        .unwrap();
    assert!(handler.0.load(Ordering::SeqCst));
}

#[derive(Default)]
struct TrackingHandler {
    active: AtomicUsize,
    maximum: AtomicUsize,
    build_active: AtomicUsize,
    build_maximum: AtomicUsize,
    per_app: Mutex<HashMap<String, usize>>,
    per_app_max: Mutex<HashMap<String, usize>>,
    generations: Mutex<HashMap<String, Vec<u64>>>,
    delay_ms: u64,
}

#[async_trait]
impl OperationHandler for TrackingHandler {
    async fn execute(
        &self,
        operation: &Operation,
        _: &CancellationToken,
    ) -> Result<(), (&'static str, &'static str)> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(active, Ordering::SeqCst);
        if operation.kind == OperationKind::Build {
            let builds = self.build_active.fetch_add(1, Ordering::SeqCst) + 1;
            self.build_maximum.fetch_max(builds, Ordering::SeqCst);
        }
        {
            let mut current = self.per_app.lock().await;
            let count = current
                .entry(operation.application_id.to_string())
                .or_default();
            *count += 1;
            let mut maximum = self.per_app_max.lock().await;
            maximum
                .entry(operation.application_id.to_string())
                .and_modify(|v| *v = (*v).max(*count))
                .or_insert(*count);
        }
        self.generations
            .lock()
            .await
            .entry(operation.application_id.to_string())
            .or_default()
            .push(operation.generation);
        tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
        {
            let mut current = self.per_app.lock().await;
            *current
                .get_mut(&operation.application_id.to_string())
                .unwrap() -= 1;
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
        if operation.kind == OperationKind::Build {
            self.build_active.fetch_sub(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

#[tokio::test]
async fn scheduler_honors_the_separate_build_bound() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let store = Arc::new(SqliteStore::open(&path).await.unwrap());
    let mut applications = Vec::new();
    for (id, name) in [
        ("scheduler-app-04", "build-one"),
        ("scheduler-app-05", "build-two"),
    ] {
        let app = application(id, name);
        let create = store.create(&app, None, &[]).await.unwrap();
        store
            .transition_operation(
                &create.operation_id,
                WorkState::Pending,
                WorkState::Running,
                None,
            )
            .await
            .unwrap();
        store
            .transition_operation(
                &create.operation_id,
                WorkState::Running,
                WorkState::Succeeded,
                None,
            )
            .await
            .unwrap();
        applications.push(app);
    }
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true);
    let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
    for (index, app) in applications.iter().enumerate() {
        let operation_id = format!("build-operation-{index}");
        sqlx::query("INSERT INTO operations(id,application_id,generation,kind,state,created_at_ms,updated_at_ms) VALUES(?1,?2,1,'build','pending',1,1)")
            .bind(operation_id)
            .bind(app.id.as_str())
            .execute(&mut connection)
            .await
            .unwrap();
    }
    drop(connection);
    let handler = Arc::new(TrackingHandler {
        delay_ms: 25,
        ..Default::default()
    });
    OperationScheduler::new(store, handler.clone(), 2, 1)
        .run_until_idle(CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(handler.build_maximum.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn scheduler_serializes_each_application_and_honors_global_bound() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteStore::open(directory.path().join("state.db"))
            .await
            .unwrap(),
    );
    for (id, name) in [("scheduler-app-01", "one"), ("scheduler-app-02", "two")] {
        let app = application(id, name);
        store.create(&app, None, &[]).await.unwrap();
        store.replace(&app, None, 1, &[]).await.unwrap();
    }
    let handler = Arc::new(TrackingHandler {
        delay_ms: 30,
        ..Default::default()
    });
    OperationScheduler::new(store.clone(), handler.clone(), 2, 1)
        .run_until_idle(CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(handler.maximum.load(Ordering::SeqCst), 2);
    assert!(
        handler
            .per_app_max
            .lock()
            .await
            .values()
            .all(|maximum| *maximum == 1)
    );
    assert!(
        handler
            .generations
            .lock()
            .await
            .values()
            .all(|generations| generations == &[1, 2])
    );
    assert!(store.pending_operations(1).await.unwrap().is_empty());
}

struct BlockingHandler;
#[async_trait]
impl OperationHandler for BlockingHandler {
    async fn execute(
        &self,
        _: &Operation,
        cancellation: &CancellationToken,
    ) -> Result<(), (&'static str, &'static str)> {
        cancellation.cancelled().await;
        Ok(())
    }
}

#[tokio::test]
async fn cancellation_returns_running_work_to_durable_recovery() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteStore::open(directory.path().join("state.db"))
            .await
            .unwrap(),
    );
    let app = application("scheduler-app-03", "cancelled");
    let mutation = store.create(&app, None, &[]).await.unwrap();
    let scheduler = OperationScheduler::new(store.clone(), Arc::new(BlockingHandler), 1, 1);
    let cancellation = CancellationToken::new();
    let child = cancellation.clone();
    let task = tokio::spawn(async move { scheduler.run_until_idle(child).await });
    loop {
        if store.operation(&mutation.operation_id).await.unwrap().state == WorkState::Running {
            break;
        }
        tokio::task::yield_now().await;
    }
    cancellation.cancel();
    task.await.unwrap().unwrap();
    assert_eq!(
        store.operation(&mutation.operation_id).await.unwrap().state,
        WorkState::Recovery
    );
}

#[tokio::test]
async fn concurrent_dispatchers_claim_each_durable_operation_once() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        SqliteStore::open(directory.path().join("claim.db"))
            .await
            .unwrap(),
    );
    let app = application("scheduler-app-06", "single-claim");
    let mutation = store.create(&app, None, &[]).await.unwrap();
    let handler = Arc::new(TrackingHandler {
        delay_ms: 30,
        ..Default::default()
    });
    let first = OperationScheduler::new(store.clone(), handler.clone(), 1, 1);
    let second = OperationScheduler::new(store.clone(), handler.clone(), 1, 1);
    let (first_result, second_result) = tokio::join!(
        first.run_until_idle(CancellationToken::new()),
        second.run_until_idle(CancellationToken::new())
    );
    first_result.unwrap();
    second_result.unwrap();
    assert_eq!(
        handler
            .generations
            .lock()
            .await
            .get(app.id.as_str())
            .unwrap(),
        &[1]
    );
    assert_eq!(
        store.operation(&mutation.operation_id).await.unwrap().state,
        WorkState::Succeeded
    );
}
