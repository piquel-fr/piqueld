//! Reconciliation coverage using the real Docker seam and an in-memory backend.

#[path = "support/application.rs"]
mod application_fixture;
#[path = "support/docker_contract.rs"]
mod docker_contract;
#[path = "support/git.rs"]
mod git_fixture;
#[path = "support/poll.rs"]
mod poll;
#[path = "support/prebuilt.rs"]
mod prebuilt;
#[path = "support/repository.rs"]
mod repository;
#[path = "support/sqlite.rs"]
mod sqlite;

use application_fixture::TestApplications;
use async_trait::async_trait;
use piqueld::api::Actor::Daemon;
use piqueld::docker::fake::{
    FakeDocker, Gate, JobRunState, JobStart, RegistryState, observed_service,
};
use piqueld::docker::{DockerApi, DockerError, ImageSource, LocalImage, resolve_image_digest};
use piqueld::reconcile::Controller;
use piqueld::store::Store;
use piqueld_core::Sha256Digest;
use piqueld_core::manifest::{ApplicationManifest, ApplicationTemplate};
use piqueld_core::planner::PlanRequest;
use piqueld_core::resource::{
    APPLICATION_LABEL, Convergence, INSTANCE_LABEL, MANAGED_LABEL, ObservedApplication,
    ObservedNetwork, ObservedService, ObservedVolume, ResolvedApplication, ResolvedSource,
    SERVICE_LABEL, SPEC_HASH_LABEL,
};
use piqueld_core::{ApplicationId, EnvironmentId, Event, NormalizedApplication, Plan};
use piqueld_core::{Operation, OperationState};
use poll::eventually;
use prebuilt::manifest;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

/// How often tests poll for work the controller finishes in the background.
const POLL: Duration = Duration::from_millis(10);

/// An [`ImageSource`] whose references always have the same repo digests.
struct FixedImageSource(Vec<String>);

#[async_trait]
impl ImageSource for FixedImageSource {
    async fn repo_digests(&self, _reference: &str) -> Result<Option<Vec<String>>, DockerError> {
        Ok(Some(self.0.clone()))
    }

    async fn pull(&self, _reference: &str) -> Result<(), DockerError> {
        Ok(())
    }
}

/// Degrades `docker`'s running web service of `id` and waits for the
/// controller to repair it.
async fn assert_active_repair(docker: &FakeDocker, id: &EnvironmentId) {
    {
        let mut observed = docker.observed.lock().await;
        let service = observed
            .services
            .iter_mut()
            .find(|service| service.labels.get(APPLICATION_LABEL) == Some(&id.to_string()))
            .unwrap();
        service.replicas = 9;
        service.convergence = Convergence::Degraded;
    }
    eventually(Duration::from_secs(3), POLL, async || {
        (docker.observe(id).await.unwrap().services[0].replicas == 1).then_some(())
    })
    .await
    .expect("active target is repaired while candidate image pull is blocked");
}

/// The environment created with `application`, which shares its ID.
fn environment(application: &NormalizedApplication) -> EnvironmentId {
    EnvironmentId::default_for(application.id())
}

/// Labels of the same application that another installation owns.
fn foreign_labels(application_id: &EnvironmentId) -> BTreeMap<String, String> {
    BTreeMap::from([
        (MANAGED_LABEL.into(), "true".into()),
        (INSTANCE_LABEL.into(), "other-instance".into()),
        (APPLICATION_LABEL.into(), application_id.to_string()),
        (SPEC_HASH_LABEL.into(), format!("sha256:{}", "b".repeat(64))),
    ])
}

/// `manifest` with its first service mounting stored secret `token`.
fn with_token(mut manifest: ApplicationManifest) -> ApplicationManifest {
    manifest.spec.services[0]
        .secrets
        .push(piqueld_core::manifest::SecretMount {
            name: "token".into(),
            target: "/run/secrets/token".into(),
        });
    manifest
}

/// The fixture manifest with before-rollout jobs that reuse its `web` service.
fn manifest_with_jobs(jobs: &[&str], timeout_seconds: u32) -> ApplicationManifest {
    let mut input = manifest();
    input.spec.jobs = jobs
        .iter()
        .map(|name| piqueld_core::manifest::Job {
            name: (*name).into(),
            service: "web".into(),
            command: vec!["notes".into(), (*name).into()],
            environment: std::collections::BTreeMap::new(),
            secrets: Vec::new(),
            run: piqueld_core::manifest::JobRun::BeforeRollout,
            timeout_seconds,
            volume_access: piqueld_core::manifest::JobVolumeAccess::ReadOnly,
        })
        .collect();
    input
}

/// A migration whose database depends on storage, plus an unrelated service.
fn manifest_with_job_dependencies() -> ApplicationManifest {
    let mut input = manifest_with_jobs(&["migrate"], 300);
    input.spec.services[0].depends_on = vec!["db".into()];
    for (name, dependencies) in [
        ("db", vec!["storage".into()]),
        ("storage", vec![]),
        ("other", vec![]),
    ] {
        let mut service = input.spec.services[0].clone();
        service.name = name.into();
        service.depends_on = dependencies;
        service.mounts.clear();
        service.healthcheck = Some(piqueld_core::manifest::HealthCheck::Command {
            command: vec!["ready".into()],
            interval_seconds: 1.into(),
            timeout_seconds: 1.into(),
            start_period_seconds: 0.into(),
            retries: 3.into(),
        });
        input.spec.services.push(service);
    }
    input
}

/// The fixture with one exclusive migration of `web`, which mounts `data`.
fn manifest_with_exclusive_job() -> ApplicationManifest {
    let mut input = manifest_with_jobs(&["migrate"], 300);
    input.spec.jobs[0].volume_access = piqueld_core::manifest::JobVolumeAccess::Exclusive;
    input
}

/// A store, the prebuilt fixture resolved for it, and a controller over a
/// fake Docker backend.
struct ControllerHarness {
    _directory: tempfile::TempDir,
    database_path: PathBuf,
    store: Arc<Store>,
    application: NormalizedApplication,
    resolved: ResolvedApplication,
    docker: Arc<FakeDocker>,
    controller: Controller<FakeDocker>,
}

impl ControllerHarness {
    async fn new() -> Self {
        Self::with_docker(FakeDocker::default()).await
    }

    async fn with_docker(docker: FakeDocker) -> Self {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database_path = directory.path().join("control-plane.db");
        let store = Arc::new(
            Store::open(&database_path)
                .await
                .expect("fresh database opens"),
        );
        let application = prebuilt::application("app-fake-docker-01");
        let resolved = prebuilt::resolve(&application, store.instance_id());
        let docker = Arc::new(docker);
        let controller = Controller::new(Arc::clone(&docker), Arc::clone(&store));
        Self {
            _directory: directory,
            database_path,
            store,
            application,
            resolved,
            docker,
            controller,
        }
    }

    /// Replaces the application, resolving it again.
    fn set_application(&mut self, application: NormalizedApplication) {
        self.resolved = prebuilt::resolve(&application, self.store.instance_id());
        self.application = application;
    }

    /// Gives the controller `timeout` to wait for each convergence.
    fn set_convergence_timeout(&mut self, timeout: Duration) {
        self.controller = Controller::new(Arc::clone(&self.docker), Arc::clone(&self.store))
            .with_retry_policy(piqueld::reconcile::RetryPolicy {
                convergence_timeout: timeout,
                ..piqueld::reconcile::RetryPolicy::default()
            });
    }

    fn applications(&self) -> TestApplications {
        TestApplications::new(
            Arc::clone(&self.store),
            self.controller
                .runtime(Arc::new(tokio::sync::Notify::new())),
        )
    }

    /// Saves and deploys `manifest`, expecting generation `expected`.
    async fn apply(&self, manifest: ApplicationManifest, expected: Option<u64>) -> Operation {
        // Boxed: accepting a mutation holds a whole manifest across awaits.
        Box::pin(
            self.applications()
                .apply(manifest.validate_template().unwrap(), expected),
        )
        .await
        .unwrap()
    }

    /// Saves `manifest` without deploying, expecting generation `expected`.
    async fn save(
        &self,
        manifest: ApplicationManifest,
        expected: Option<u64>,
    ) -> piqueld_core::api::SavedApplication {
        Box::pin(
            self.applications()
                .save(manifest.validate_template().unwrap(), expected),
        )
        .await
        .unwrap()
    }

    /// Journals the application with its resolved target.
    async fn create(&self) -> Operation {
        self.store
            .save_application(
                &ApplicationTemplate::from(&self.application),
                Some(&self.resolved),
                None,
            )
            .await
            .expect("application saved")
    }

    /// Stores a value for the application's secret `token`, replacing
    /// version `generation`.
    async fn store_token(&self, application: &EnvironmentId, generation: i64, value: &[u8]) {
        self.store
            .put_stored_secret(
                Daemon,
                &ApplicationId::parse(application.as_str()).unwrap(),
                "token",
                generation,
                value.to_vec(),
                None,
            )
            .await
            .unwrap();
    }

    /// Runs one reconciliation scan.
    async fn scan(&self) {
        self.controller
            .scan(&CancellationToken::new())
            .await
            .unwrap();
    }

    /// Scans once and expects `operation` to have succeeded.
    async fn finish(&self, operation: &Operation) {
        self.scan().await;
        let recorded = self.operation(&operation.id).await;
        assert_eq!(recorded.state, OperationState::Succeeded, "{recorded:?}");
    }

    /// Scans once and returns the operation's resulting state and error code.
    async fn scan_result(&self, operation: &Operation) -> (OperationState, Option<String>) {
        self.scan().await;
        let operation = self.operation(&operation.id).await;
        (operation.state, operation.error_code)
    }

    async fn operation(&self, id: &str) -> Operation {
        self.store.operation(id).await.unwrap()
    }

    /// The events operation `id` recorded.
    async fn operation_events(&self, id: &str) -> Vec<Event> {
        self.store
            .filtered_events(
                &piqueld_core::observability::EventFilter {
                    operation_id: Some(id.to_owned()),
                    ..Default::default()
                },
                &piqueld::store::Visibility::ALL,
                None,
                100,
            )
            .await
            .unwrap()
            .items
    }

    /// The target environment `id` runs.
    async fn target(&self, id: &EnvironmentId) -> ResolvedApplication {
        self.store.get(id).await.unwrap().resolved.unwrap()
    }

    /// Image pulls so far, across references.
    async fn pulls(&self) -> u64 {
        self.docker.registry.lock().await.pulls.values().sum()
    }

    /// Direct access to the harness database.
    async fn database(&self) -> sqlx::SqliteConnection {
        sqlite::connect(&self.database_path).await
    }

    /// Marks `operation_id` running, as a daemon interrupted mid-operation
    /// leaves it.
    async fn interrupt(&self, operation_id: &str) {
        sqlx::query(
            "UPDATE operations SET state='running',started_at_ms=created_at_ms,updated_at_ms=created_at_ms WHERE id=?1",
        )
        .bind(operation_id)
        .execute(&mut self.database().await)
        .await
        .expect("operation can be interrupted");
    }

    /// Moves a persisted failure beyond its retry backoff without sleeping.
    async fn expire_backoff(&self, operation: &Operation) {
        sqlx::query("UPDATE operations SET updated_at_ms=1 WHERE id=?1")
            .bind(&operation.id)
            .execute(&mut self.database().await)
            .await
            .unwrap();
    }

    /// Simulates a daemon restart: reopens the database behind a fresh controller.
    async fn reopen(&mut self) {
        self.store = Arc::new(Store::open(&self.database_path).await.unwrap());
        self.controller = Controller::new(Arc::clone(&self.docker), Arc::clone(&self.store));
    }

    async fn assert_recovered(&self, operation_id: &str) {
        let status = self
            .store
            .status(&environment(&self.application))
            .await
            .expect("status is readable");
        assert_eq!(status.state, piqueld::store::ApplicationState::Ready);
        let operation = self.operation(operation_id).await;
        assert_eq!(operation.state, OperationState::Succeeded);
        let observed = self
            .docker
            .observe(&environment(&self.application))
            .await
            .expect("fake observation");
        assert_eq!(observed.volumes.len(), 1);
        assert_eq!(observed.services.len(), 1);
    }

    /// Journals the application with two `web` replicas.
    async fn replace(&self) -> Operation {
        let mut replacement = self.application.to_manifest();
        replacement.spec.services[0].replicas = 2.into();
        let replacement = replacement
            .validate()
            .unwrap()
            .normalize(self.application.id().clone());
        self.store
            .save_application(
                &ApplicationTemplate::from(&replacement),
                Some(&prebuilt::resolve(&replacement, self.store.instance_id())),
                None,
            )
            .await
            .expect("application replacement is durable")
    }

    /// The replicas Docker runs of the first service.
    async fn replicas(&self) -> u16 {
        self.docker
            .observe(&environment(&self.application))
            .await
            .unwrap()
            .services[0]
            .replicas
    }

    async fn repair_drift(&self) {
        {
            let mut observed = self.docker.observed.lock().await;
            observed.services[0].replicas = 1;
            observed.services[0].convergence = Convergence::Degraded;
        }
        self.scan().await;
        assert_eq!(self.replicas().await, 2);
    }

    async fn delete(&self) -> Operation {
        self.store
            .request_delete(&environment(&self.application))
            .await
            .expect("delete is durable")
    }

    async fn assert_deleted(&self) {
        assert!(matches!(
            self.store.get(&environment(&self.application)).await,
            Err(piqueld::store::StoreError::NotFound)
        ));
        let observed = self
            .docker
            .observe(&environment(&self.application))
            .await
            .expect("final observation");
        assert_eq!(observed.services, [] as [ObservedService; 0]);
        assert_eq!(observed.networks, [] as [ObservedNetwork; 0]);
        assert_eq!(observed.volumes.len(), 1);
    }

    /// Seeds the application's resources as Docker would run them, checks
    /// planning then finds nothing to do, and journals the application.
    async fn seed_matching_runtime(&self) -> Operation {
        self.docker
            .ensure_network(&self.resolved.networks[0])
            .await
            .expect("network is seeded");
        self.docker
            .ensure_volume(&self.resolved.volumes[0])
            .await
            .expect("volume is seeded");
        self.docker
            .ensure_service(&self.resolved.services[0])
            .await
            .expect("service is seeded");
        let observed = self
            .docker
            .observe(&environment(&self.application))
            .await
            .expect("matching observation");
        let plan = Plan::from_request(
            &PlanRequest::Reconcile {
                desired: self.resolved.clone(),
            },
            &observed,
        );
        assert_eq!(plan.actions, [] as [piqueld_core::PlanAction; 0]);
        self.create().await
    }

    /// Journals the application over Docker resources another installation
    /// already created, as `foreign` adds them, and asserts one scan journals
    /// the conflict as a degraded, failed operation, leaving them untouched.
    async fn assert_refuses_foreign(&self, foreign: impl FnOnce(&mut ObservedApplication)) {
        foreign(&mut *self.docker.observed.lock().await);
        let foreign = self.docker.observed.lock().await.clone();
        let created = self.create().await;
        self.scan().await;
        let status = self
            .store
            .status(&environment(&self.application))
            .await
            .expect("status is readable");
        assert_eq!(status.state, piqueld::store::ApplicationState::Degraded);
        assert_eq!(
            self.operation(&created.id).await.state,
            OperationState::Failed
        );
        let observed = self.docker.observed.lock().await;
        // Nothing replaced them, and nothing else appeared beside them.
        assert_eq!(observed.services.len(), foreign.services.len());
        assert_eq!(observed.networks.len(), foreign.networks.len());
        assert_eq!(observed.volumes.len(), foreign.volumes.len());
        assert!(
            foreign
                .services
                .iter()
                .all(|service| observed.services.contains(service))
        );
        assert!(
            foreign
                .networks
                .iter()
                .all(|network| observed.networks.contains(network))
        );
        assert!(
            foreign
                .volumes
                .iter()
                .all(|volume| observed.volumes.contains(volume))
        );
    }

    /// Names of the jobs started so far, in start order.
    async fn started_jobs(&self) -> Vec<String> {
        self.docker
            .job_starts
            .lock()
            .await
            .iter()
            .map(|start| start.job.clone())
            .collect()
    }

    /// Waits until `count` job runs have been started.
    async fn wait_for_job_starts(&self, count: usize) {
        eventually(Duration::from_secs(10), POLL, async || {
            (self.docker.job_starts.lock().await.len() >= count).then_some(())
        })
        .await
        .expect("job starts");
    }

    /// Fails a job and its cleanup, leaving the cleanup pending for scans.
    async fn leave_job_cleanup_pending(&self) {
        self.docker.job_exit.store(3, Ordering::SeqCst);
        self.docker.fail_job_cleanup.store(true, Ordering::SeqCst);
        let failed = self
            .apply(manifest_with_jobs(&["migrate"], 300), None)
            .await;
        assert_eq!(
            self.scan_result(&failed).await,
            (OperationState::Failed, Some("job_failed".into()))
        );
        self.docker.fail_job_cleanup.store(false, Ordering::SeqCst);
    }

    /// Scans once, running `meanwhile` after discovery, while the scan
    /// retries pending job cleanup.
    async fn scan_during_cleanup(&self, meanwhile: impl Future<Output = ()>) {
        let cleanups = self.docker.job_cleanups.load(Ordering::SeqCst);
        let held = self.docker.hold_job_cleanup.lock().await;
        let shutdown = CancellationToken::new();
        let scan = self.controller.scan(&shutdown);
        tokio::pin!(scan);
        tokio::select! {
            result = &mut scan => panic!("scan finished while cleanup was held: {result:?}"),
            retrying = eventually(Duration::from_secs(10), POLL, async || {
                (self.docker.job_cleanups.load(Ordering::SeqCst) > cleanups).then_some(())
            }) => retrying.expect("the scan retries the failed run's cleanup"),
        }
        meanwhile.await;
        drop(held);
        scan.await.unwrap();
    }

    /// Adds a service of another installation named like `operation`'s
    /// prepared service `name`, a conflict blocking its plan, and returns it.
    async fn add_foreign_service(&self, operation: &Operation, name: &str) -> ObservedService {
        let desired = self
            .store
            .prepared_target(&operation.id)
            .await
            .unwrap()
            .unwrap()
            .services
            .into_iter()
            .find(|service| service.logical_name.as_str() == name)
            .unwrap();
        let mut labels = foreign_labels(&operation.environment_id);
        labels.insert(SERVICE_LABEL.into(), name.into());
        let foreign = ObservedService {
            labels,
            ..observed_service(&desired)
        };
        self.docker
            .observed
            .lock()
            .await
            .services
            .push(foreign.clone());
        foreign
    }

    /// Removes `foreign`, resolving its conflict.
    async fn remove_foreign_service(&self, foreign: &ObservedService) {
        self.docker
            .observed
            .lock()
            .await
            .services
            .retain(|service| service != foreign);
    }

    /// The logical names of the services this installation runs, sorted.
    async fn owned_services(&self) -> Vec<String> {
        let mut names = self
            .docker
            .observed
            .lock()
            .await
            .services
            .iter()
            .filter(|service| {
                service.labels.get(INSTANCE_LABEL).map(String::as_str) != Some("other-instance")
            })
            .map(|service| service.labels[SERVICE_LABEL].clone())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    /// The application's newest build record and its retained output.
    async fn latest_run(
        &self,
        application: &EnvironmentId,
    ) -> (piqueld_core::api::BuildRecord, String) {
        let run = self
            .store
            .builds(
                None,
                Some(application),
                &piqueld_core::access::Scope::All,
                None,
                10,
            )
            .await
            .unwrap()
            .items
            .remove(0);
        let output = self
            .store
            .build_logs(run.id, None, None)
            .await
            .unwrap()
            .items
            .into_iter()
            .map(|line| line.text)
            .collect();
        (run, output)
    }

    async fn assert_git_build_history(&self, id: &EnvironmentId) {
        let builds = self
            .store
            .builds(None, Some(id), &piqueld_core::access::Scope::All, None, 50)
            .await
            .unwrap();
        assert_eq!(
            builds.items.len(),
            3,
            "one record per executed preparation, not per accepted/replayed request"
        );
        assert_eq!(builds.items[0].state, piqueld_core::api::BuildState::Failed);
        assert!(
            builds
                .items
                .iter()
                .all(|b| b.commit.is_some() && b.log_bytes > 0)
        );
        assert!(
            builds.items[1..].iter().all(
                |b| b.state == piqueld_core::api::BuildState::Succeeded && b.image_id.is_some()
            )
        );
        assert!(
            self.store
                .build_logs(builds.items[0].id, None, None)
                .await
                .unwrap()
                .items
                .iter()
                .any(|chunk| chunk.text.contains("Build failed"))
        );
    }
}

/// Two independently deployed environments exercising the shared Docker backend.
struct EnvironmentHarness {
    harness: ControllerHarness,
    applications: TestApplications,
    production: EnvironmentId,
    staging: EnvironmentId,
    generation: u64,
}

impl EnvironmentHarness {
    async fn new() -> Self {
        use piqueld::api::{Mutation, MutationResponse};
        use piqueld_core::EnvironmentName;
        let mut harness = ControllerHarness::new().await;
        harness.application = with_token(harness.application.to_manifest())
            .validate()
            .unwrap()
            .normalize(harness.application.id().clone());
        let store = &harness.store;
        let applications = harness.applications();
        let saved = store
            .save_application(&ApplicationTemplate::from(&harness.application), None, None)
            .await
            .unwrap();
        let production = environment(&harness.application);
        // Boxed: accepting a mutation holds a whole manifest across awaits.
        let MutationResponse::Environment(staging) = Box::pin(applications.accept(
            Daemon,
            Mutation::CreateEnvironment {
                application: harness.application.id().clone(),
                name: EnvironmentName::parse("staging").unwrap(),
                source: None,
            },
            Some(saved.generation),
            false,
            None,
        ))
        .await
        .unwrap() else {
            panic!("created staging")
        };
        // Creating an environment bumps the application generation.
        let generation = saved.generation + 1;
        // One stored value; each environment mounts its own Docker copy.
        harness.store_token(&production, 0, b"shared token").await;
        for id in [&production, &staging.id] {
            applications.deploy(id, Some(generation)).await.unwrap();
        }
        harness.scan().await;
        Self {
            harness,
            applications,
            production,
            staging: staging.id,
            generation,
        }
    }

    async fn delete_staging(&self) {
        use piqueld::api::Mutation;
        self.applications
            .accept(
                Daemon,
                Mutation::Delete {
                    id: self.staging.clone(),
                },
                Some(self.generation),
                false,
                None,
            )
            .await
            .unwrap();
        self.harness.scan().await;
    }
}

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
async fn deleting_staging_preserves_production_runtime_secrets_and_history() {
    let fixture = Box::pin(EnvironmentHarness::new()).await;
    let harness = &fixture.harness;
    let store = &harness.store;
    let production = &fixture.production;
    let staging = &fixture.staging;
    let production_runtime = harness.docker.observe(production).await.unwrap();
    let staging_runtime = harness.docker.observe(staging).await.unwrap();
    assert_eq!(production_runtime.services.len(), 1);
    assert_eq!(staging_runtime.services.len(), 1);
    assert_ne!(
        production_runtime.services[0].name,
        staging_runtime.services[0].name
    );
    assert_ne!(
        production_runtime.volumes[0].name,
        staging_runtime.volumes[0].name
    );
    let production_history = store.deployments(production, None, 10).await.unwrap();
    let production_events = store.events(Some(production), None, 100).await.unwrap();
    assert!(!production_history.items.is_empty());
    assert_eq!(
        store
            .deployments(staging, None, 10)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    assert_eq!(harness.docker.secrets.lock().await.len(), 2);

    fixture.delete_staging().await;

    assert!(matches!(
        store.get(staging).await,
        Err(piqueld::store::StoreError::NotFound)
    ));
    let remaining = harness.docker.observe(staging).await.unwrap();
    assert_eq!(remaining.services, [] as [ObservedService; 0]);
    assert_eq!(remaining.networks, [] as [ObservedNetwork; 0]);
    assert_eq!(
        remaining.volumes, staging_runtime.volumes,
        "staging volume data is retained"
    );
    assert_eq!(
        harness.docker.observe(production).await.unwrap(),
        production_runtime
    );
    assert_eq!(
        store.status(production).await.unwrap().state,
        piqueld::store::ApplicationState::Ready
    );
    let history = store.deployments(production, None, 10).await.unwrap();
    assert_eq!(
        history
            .items
            .iter()
            .map(|item| (&item.operation.id, &item.application))
            .collect::<Vec<_>>(),
        production_history
            .items
            .iter()
            .map(|item| (&item.operation.id, &item.application))
            .collect::<Vec<_>>()
    );
    let events = store.events(Some(production), None, 100).await.unwrap();
    assert!(
        production_events
            .items
            .iter()
            .all(|old| events.items.iter().any(|event| event.id == old.id))
    );
    // Staging's events stay in the application's history until it is deleted.
    let staging_events = store.events(Some(staging), None, 100).await.unwrap();
    assert!(
        staging_events
            .items
            .iter()
            .any(|event| event.environment_id.as_ref() == Some(staging))
    );
    let stored = store
        .stored_secrets(harness.application.id())
        .await
        .unwrap();
    assert_eq!(stored[0].metadata.generation, 1);
    let values = harness.docker.secrets.lock().await;
    assert_eq!(values.len(), 1, "only staging's copy is removed");
    assert_eq!(values.values().next().unwrap().value, b"shared token");
    let environments = store.environments(harness.application.id()).await;
    assert_eq!(environments.unwrap().len(), 1);
    assert_eq!(
        store
            .application(harness.application.id())
            .await
            .unwrap()
            .generation,
        fixture.generation + 1,
        "deleting staging bumps the generation once"
    );
}

/// Deploys `auth`, whose `web` service is exported to `notes` beside an
/// unexported `worker`, and `notes`, which imports it, then scans until they
/// are linked. Returns the harness and both environments.
async fn linked_harness() -> (ControllerHarness, EnvironmentId, EnvironmentId) {
    let harness = ControllerHarness::new().await;
    let deploy = |name: &str, edit: fn(&mut ApplicationManifest)| {
        let mut manifest = harness.application.to_manifest();
        manifest.metadata.name = name.into();
        edit(&mut manifest);
        let application = manifest
            .validate()
            .unwrap()
            .normalize(ApplicationId::parse(format!("app-{name}-0001")).unwrap());
        let resolved = prebuilt::resolve(&application, harness.store.instance_id());
        let store = Arc::clone(&harness.store);
        async move {
            store
                .save_application(
                    &ApplicationTemplate::from(&application),
                    Some(&resolved),
                    None,
                )
                .await
                .unwrap();
            resolved.id
        }
    };
    let auth = deploy("auth", |manifest| {
        manifest.spec.exports.push(piqueld_core::manifest::Export {
            service: "web".into(),
            applications: vec!["notes".into()],
            previews: false,
        });
        let mut worker = manifest.spec.services[0].clone();
        worker.name = "worker".into();
        manifest.spec.services.push(worker);
    })
    .await;
    let notes = deploy("notes", |manifest| {
        manifest.spec.imports.push(piqueld_core::manifest::Import {
            application: "auth".into(),
            service: "web".into(),
            environment: None,
        });
    })
    .await;
    for _ in 0..3 {
        harness.scan().await;
    }

    (harness, auth, notes)
}

/// `observed`'s `web` service.
fn web(observed: &ObservedApplication) -> ObservedService {
    observed
        .services
        .iter()
        .find(|service| service.labels.get(SERVICE_LABEL).map(String::as_str) == Some("web"))
        .unwrap()
        .clone()
}

/// Two applications linked by an export and an import connect once both
/// deploy: the importer creates the link network, and the exporter's service
/// joins it on its next scan under `<service>.<application>`.
#[tokio::test]
async fn linked_applications_join_the_importers_network() {
    use piqueld_core::links::ImportState;
    let (harness, auth, notes) = linked_harness().await;
    let network = piqueld_core::DockerNetworkName::for_link(&notes, &auth);
    let importer = harness.docker.observe(&notes).await.unwrap();
    assert!(
        importer
            .networks
            .iter()
            .any(|observed| observed.name == network.as_str())
    );
    assert!(importer.services[0].networks.iter().any(|attachment| {
        attachment.network == network.as_str() && attachment.aliases.is_empty()
    }));
    let exporter = harness.docker.observe(&auth).await.unwrap();
    assert!(
        web(&exporter)
            .networks
            .contains(&piqueld_core::resource::NetworkAttachment {
                network: network.to_string(),
                aliases: vec!["web.auth".into()],
            })
    );
    assert!(
        exporter
            .services
            .iter()
            .filter(|service| service.name != web(&exporter).name)
            .all(|service| service
                .networks
                .iter()
                .all(|attachment| attachment.network != network.as_str())),
        "only the exported service joins"
    );
    let report = harness
        .store
        .link_report(&notes, None, &piqueld_core::access::Scope::All)
        .await
        .unwrap();
    assert_eq!(report.imports[0].state, ImportState::Connected);
    let report = harness
        .store
        .link_report(&auth, None, &piqueld_core::access::Scope::All)
        .await
        .unwrap();
    assert_eq!(report.importers[0].application.as_str(), "notes");
}

/// auth's worker fails, which blocks its plan. Deleting notes neither waits
/// for auth nor leaves web attached: web leaves the link network, which is
/// removed once nothing uses it.
#[tokio::test]
async fn revoked_links_close_while_the_exporters_plan_is_blocked() {
    let (harness, auth, notes) = linked_harness().await;
    let network = piqueld_core::DockerNetworkName::for_link(&notes, &auth);
    for service in &mut harness.docker.observed.lock().await.services {
        if service.labels.get(SERVICE_LABEL).map(String::as_str) == Some("worker") {
            service.convergence = Convergence::Failed;
        }
    }
    harness.store.request_delete(&notes).await.unwrap();
    for _ in 0..3 {
        harness.scan().await;
    }
    assert!(matches!(
        harness.store.get(&notes).await,
        Err(piqueld::store::StoreError::NotFound)
    ));
    let exporter = harness.docker.observe(&auth).await.unwrap();
    assert!(
        web(&exporter)
            .networks
            .iter()
            .all(|attachment| attachment.network != network.as_str())
    );
    assert_eq!(exporter.linked_networks, []);
    assert!(
        exporter
            .services
            .iter()
            .any(|service| service.convergence == Convergence::Failed),
        "auth's plan stayed blocked"
    );
}

#[tokio::test]
async fn deleting_an_exporter_removes_networks_its_deleted_importers_left() {
    let (harness, auth, notes) = linked_harness().await;
    let network = piqueld_core::DockerNetworkName::for_link(&notes, &auth);
    let scan = || async {
        for _ in 0..3 {
            harness.scan().await;
        }
    };
    let leftover = harness
        .docker
        .observed
        .lock()
        .await
        .networks
        .iter()
        .find(|observed| observed.name == network.as_str())
        .cloned()
        .unwrap();
    harness.store.request_delete(&notes).await.unwrap();
    scan().await;
    // As if auth's deletion came before the full scan that removes it.
    harness.docker.observed.lock().await.networks.push(leftover);
    harness.store.request_delete(&auth).await.unwrap();
    scan().await;
    assert!(matches!(
        harness.store.get(&auth).await,
        Err(piqueld::store::StoreError::NotFound)
    ));
    assert!(
        harness
            .docker
            .observed
            .lock()
            .await
            .networks
            .iter()
            .all(|observed| observed.name != network.as_str())
    );
}

/// A granted environment's receiver gets its socket directory mounted, as
/// the daemon's user, on the next scan without a deployment. A renamed
/// receiver gets a directory of its own, and the old one's is removed with
/// its service. Revoking the grant unmounts it and removes the directories.
#[tokio::test]
async fn telemetry_sockets_are_mounted_while_the_environment_is_granted() {
    let mut harness = ControllerHarness::new().await;
    let data = tempfile::tempdir().unwrap();
    harness.controller = Controller::new(Arc::clone(&harness.docker), Arc::clone(&harness.store))
        .with_telemetry(Some(piqueld::telemetry::Sockets::new(data.path())));
    let mut manifest = harness.application.to_manifest();
    manifest.spec.services[0].receives_telemetry = true;
    let id = harness.application.id().clone();
    harness.set_application(manifest.clone().validate().unwrap().normalize(id.clone()));
    let created = harness.create().await;
    harness.finish(&created).await;
    let environment = environment(&harness.application);
    let mounted = async |harness: &ControllerHarness| {
        harness.docker.observe(&environment).await.unwrap().services[0]
            .telemetry
            .clone()
    };
    assert_eq!(mounted(&harness).await, None, "not granted yet");

    let grant = |harness: &ControllerHarness, granted| {
        let mutation = piqueld::api::Mutation::SetTelemetry {
            id: environment.clone(),
            granted,
        };
        let applications = harness.applications();
        async move {
            Box::pin(applications.accept(Daemon, mutation, None, false, None))
                .await
                .unwrap();
        }
    };
    grant(&harness, true).await;
    harness.scan().await;
    let sockets = data.path().join("telemetry").join(environment.as_str());
    let directory = sockets.join(manifest.spec.services[0].name.as_str());
    let mount = mounted(&harness)
        .await
        .expect("granted receivers mount their socket");
    assert_eq!(mount.source, directory.display().to_string());
    assert_ne!(mount.user, "");
    assert!(directory.is_dir());

    manifest.spec.services[0].name = "otel".into();
    harness.set_application(manifest.validate().unwrap().normalize(id));
    let renamed = harness.create().await;
    harness.finish(&renamed).await;
    harness.scan().await;
    let mount = mounted(&harness)
        .await
        .expect("the renamed receiver mounts its own");
    assert_eq!(mount.source, sockets.join("otel").display().to_string());
    assert!(
        !directory.exists(),
        "the old receiver's directory is removed"
    );

    harness.scan().await;
    grant(&harness, false).await;
    harness.scan().await;
    assert_eq!(mounted(&harness).await, None, "revoking unmounts it");
    harness.scan().await;
    assert!(!sockets.exists(), "and removes the directories");
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
            .get(&environment(&harness.application))
            .await
            .unwrap()
            .delete_intent()
    );
    harness
        .docker
        .deny_network_removal
        .store(false, Ordering::Relaxed);
    harness.scan().await;
    harness.assert_deleted().await;
    assert!(matches!(
        harness.store.operation(&deletion.id).await,
        Err(piqueld::store::StoreError::NotFound)
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
        .status(&environment(&harness.application))
        .await
        .unwrap();
    assert_eq!(status.state, piqueld::store::ApplicationState::Ready);
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

#[tokio::test]
async fn controller_refuses_a_foreign_same_name_service() {
    let harness = ControllerHarness::new().await;
    let mut labels = foreign_labels(&environment(&harness.application));
    labels.insert(SERVICE_LABEL.into(), "web".into());
    let foreign = ObservedService {
        labels,
        ..observed_service(&harness.resolved.services[0])
    };
    harness
        .assert_refuses_foreign(|observed| observed.services.push(foreign))
        .await;
}

#[tokio::test]
async fn controller_refuses_a_foreign_same_name_network() {
    let harness = ControllerHarness::new().await;
    let foreign = ObservedNetwork {
        name: harness.resolved.networks[0].name.to_string(),
        runtime_configuration_matches: true,
        labels: foreign_labels(&environment(&harness.application)),
        in_use: false,
    };
    harness
        .assert_refuses_foreign(|observed| observed.networks.push(foreign))
        .await;
}

#[tokio::test]
async fn controller_refuses_a_foreign_same_name_volume() {
    let harness = ControllerHarness::new().await;
    let foreign = ObservedVolume {
        name: harness.resolved.volumes[0].name.to_string(),
        runtime_configuration_matches: true,
        labels: foreign_labels(&environment(&harness.application)),
    };
    harness
        .assert_refuses_foreign(|observed| observed.volumes.push(foreign))
        .await;
}

#[tokio::test]
async fn image_resolution_repairs_a_single_tag_flip_through_a_retry() {
    let docker = FakeDocker::default();
    docker.arm_tag_flips(1).await;
    // Pulling a pinned reference leaves the flip armed.
    let pinned = format!("ghcr.io/example/notes@sha256:{}", "c".repeat(64));
    assert_eq!(docker.resolve_image(&pinned).await.unwrap(), pinned);

    let resolved = docker
        .resolve_image("ghcr.io/example/notes:1.4.0")
        .await
        .expect("a single tag flip converges through the bounded retry");

    assert_eq!(
        resolved,
        format!(
            "ghcr.io/example/notes@sha256:{}",
            RegistryState::flipped_digest()
        )
    );
    let registry = docker.registry.lock().await;
    assert_eq!(
        registry.pulls.get("ghcr.io/example/notes:1.4.0"),
        Some(&2),
        "the flipped resolution must retry the whole pull exactly once"
    );
}

#[tokio::test]
async fn image_resolution_preserves_a_requested_digest() {
    let requested = format!("ghcr.io/example/notes@sha256:{}", "b".repeat(64));
    let source = FixedImageSource(vec![
        format!("ghcr.io/example/notes@sha256:{}", "a".repeat(64)),
        requested.clone(),
    ]);

    assert_eq!(
        resolve_image_digest(&source, &requested).await.unwrap(),
        requested
    );
}

#[tokio::test]
async fn image_resolution_fails_sanitized_when_the_tag_never_settles() {
    let docker = FakeDocker::default();
    docker.arm_tag_flips(usize::MAX).await;

    let error = docker
        .resolve_image("ghcr.io/example/notes:1.4.0")
        .await
        .expect_err("an always-flipping tag never converges");

    assert!(matches!(
        error,
        DockerError::ImageResolution("confirm stable image digest")
    ));
}

#[tokio::test]
async fn save_and_deploy_are_durable_before_resolution_and_reconcile_reuses_images() {
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let manifest = manifest();
    let accepted = harness.apply(manifest.clone(), Some(0)).await;
    assert_eq!(harness.pulls().await, 0);
    let stored = harness.store.get(&accepted.environment_id).await.unwrap();
    assert_eq!(stored.application.generation, 1);
    assert!(stored.resolved.is_none());
    harness.finish(&accepted).await;
    let pulls = harness.pulls().await;
    let repeated = harness.save(manifest.clone(), Some(1)).await;
    assert!(repeated.operation_id.is_none());
    assert_eq!(repeated.generation, 2);
    assert_eq!(harness.pulls().await, pulls);
    let reconcile = applications
        .reconcile(&accepted.environment_id, Some(2))
        .await
        .unwrap();
    harness.finish(&reconcile).await;
    assert_eq!(harness.pulls().await, pulls);
    let refresh = applications
        .deploy(&accepted.environment_id, Some(2))
        .await
        .unwrap();
    assert_ne!(refresh.id, accepted.id);
    harness.finish(&refresh).await;
    assert!(harness.pulls().await > pulls);
    assert_eq!(
        harness
            .store
            .get(&accepted.environment_id)
            .await
            .unwrap()
            .application
            .generation,
        2
    );
    let events = harness
        .store
        .events(Some(&accepted.environment_id), None, 100)
        .await
        .unwrap()
        .items;
    assert!(
        events
            .iter()
            .any(|event| event.kind == "operation_succeeded" && event.attempt == Some(1))
    );
    assert!(
        events
            .iter()
            .any(|event| event.kind == "operation_succeeded" && event.attempt == Some(2))
    );
}

#[tokio::test]
async fn generations_protect_full_replacement_and_deletion_without_merging() {
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let mut manifest = manifest();
    let first = harness.apply(manifest.clone(), Some(0)).await;
    manifest.spec.services[0]
        .environment
        .insert("CHANGED".into(), "yes".into());
    let second = harness.apply(manifest.clone(), Some(1)).await;
    assert_eq!(second.generation, 2);
    assert!(matches!(
        applications
            .apply(manifest.clone().validate_template().unwrap(), Some(1))
            .await,
        Err(piqueld::api::ApplicationError::Store(
            piqueld::store::StoreError::GenerationConflict {
                expected: 1,
                actual: 2
            }
        ))
    ));
    assert!(
        applications
            .delete(&first.environment_id, Some(1))
            .await
            .is_err()
    );
    manifest.spec.services[0].environment.clear();
    let third = harness.apply(manifest.clone(), Some(2)).await;
    assert!(
        harness
            .store
            .get(&first.environment_id)
            .await
            .unwrap()
            .manifest()
            .unwrap()
            .spec()
            .services[0]
            .environment
            .is_empty()
    );
    assert_eq!(third.generation, 3);
    let deletion = applications
        .delete(&first.environment_id, Some(3))
        .await
        .unwrap();
    assert_eq!(deletion.generation, 4);
    assert_eq!(
        applications
            .delete(&first.environment_id, None)
            .await
            .unwrap()
            .id,
        deletion.id
    );
    assert!(
        applications
            .deploy(&first.environment_id, None)
            .await
            .is_err()
    );
    assert!(matches!(
        applications
            .apply(manifest.validate_template().unwrap(), Some(4))
            .await,
        Err(piqueld::api::ApplicationError::Store(
            piqueld::store::StoreError::IllegalTransition
        ))
    ));
    let deleting = harness.store.get(&first.environment_id).await.unwrap();
    assert_eq!(deleting.application.generation, 4);
    assert!(deleting.delete_intent());
    assert_eq!(
        harness.operation(&deletion.id).await.state,
        OperationState::Requested
    );
}

#[tokio::test]
async fn periodic_recovery_reuses_failed_prepared_target_and_records_health_changes() {
    let harness = ControllerHarness::new().await;
    let operation = harness.create().await;
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
    harness
        .store
        .transition_operation(
            &operation.id,
            OperationState::Running,
            OperationState::Failed,
            Some(("docker_unavailable", "Docker is unavailable")),
        )
        .await
        .unwrap();
    harness.expire_backoff(&operation).await;
    harness.finish(&operation).await;
    assert_eq!(harness.pulls().await, 0);
    let completed = harness.operation(&operation.id).await;
    assert_eq!(completed.attempt, 2);
    let observed = harness
        .docker
        .observe(&operation.environment_id)
        .await
        .unwrap();
    harness
        .store
        .record_health(&operation.id, &observed)
        .await
        .unwrap();
    harness
        .store
        .record_health(&operation.id, &observed)
        .await
        .unwrap();
    let events = harness.store.events(None, None, 100).await.unwrap().items;
    assert_eq!(
        events
            .iter()
            .filter(
                |event| event.kind == "health_changed" && event.message.as_deref() == Some("ready")
            )
            .count(),
        1
    );
    assert!(
        events
            .iter()
            .any(|event| event.kind == "operation_failed" && event.attempt == Some(1))
    );
}

#[tokio::test]
async fn health_is_degraded_while_an_expected_service_is_missing() {
    let harness = ControllerHarness::new().await;
    let operation = harness.create().await;
    harness.finish(&operation).await;
    let mut observed = harness
        .docker
        .observe(&operation.environment_id)
        .await
        .unwrap();
    // Only an unexpected, converged service runs; the target's is missing.
    observed.services[0].name = "unexpected".into();

    harness
        .store
        .record_health(&operation.id, &observed)
        .await
        .unwrap();

    let status = harness
        .store
        .status(&operation.environment_id)
        .await
        .unwrap();
    assert_eq!(status.runtime_health.as_deref(), Some("degraded"));
    assert!(status.health_observed_at_ms.is_some());
}

#[tokio::test]
async fn pending_pulls_do_not_block_other_apps_and_superseded_preparation_is_discarded() {
    let gate = Arc::new(Gate::default());
    let harness = ControllerHarness::with_docker(FakeDocker {
        resolution_gate: Some(Arc::clone(&gate)),
        ..FakeDocker::default()
    })
    .await;
    let (store, docker, controller) = (harness.store, harness.docker, harness.controller);
    let wake = Arc::new(tokio::sync::Notify::new());
    let applications =
        TestApplications::new(Arc::clone(&store), controller.runtime(Arc::clone(&wake)));
    let mut slow = manifest();
    slow.metadata.name = "slow".into();
    let original = applications
        .apply(slow.clone().validate_template().unwrap(), None)
        .await
        .unwrap();
    controller.scan(&CancellationToken::new()).await.unwrap();
    let original_target = store
        .get(&original.environment_id)
        .await
        .unwrap()
        .resolved
        .unwrap();
    slow.spec.services[0].source = piqueld_core::Source::Image {
        image: "ghcr.io/example/slow:1".into(),
    };
    let accepted = applications
        .apply(slow.validate_template().unwrap(), None)
        .await
        .unwrap();
    let cancellation = CancellationToken::new();
    let token = cancellation.clone();
    let controller_task = tokio::spawn(async move {
        controller
            .run(wake, Duration::from_millis(50), 10, 30, token)
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
        .await
        .expect("pull started");
    assert_active_repair(&docker, &accepted.environment_id).await;
    let mut fast = manifest();
    fast.metadata.name = "fast".into();
    let fast = applications
        .apply(fast.validate_template().unwrap(), None)
        .await
        .unwrap();
    eventually(Duration::from_secs(3), POLL, async || {
        (store.operation(&fast.id).await.unwrap().state == OperationState::Succeeded).then_some(())
    })
    .await
    .expect("unrelated application completes while slow pull is pending");
    let pending = store.get(&accepted.environment_id).await.unwrap();
    assert_eq!(pending.application.generation, 2);
    assert_eq!(pending.environment.resolved_generation, Some(1));
    assert_eq!(pending.resolved, Some(original_target));
    assert_eq!(
        docker
            .observe(&accepted.environment_id)
            .await
            .unwrap()
            .services[0]
            .convergence,
        Convergence::Converged
    );
    let deleted = applications
        .delete(&accepted.environment_id, None)
        .await
        .unwrap();
    eventually(Duration::from_secs(3), POLL, async || {
        matches!(
            store.operation(&deleted.id).await,
            Err(piqueld::store::StoreError::NotFound)
        )
        .then_some(())
    })
    .await
    .expect("delete supersedes pending preparation without waiting for its pull");
    assert_eq!(docker.images.maximum.load(Ordering::SeqCst), 2);
    cancellation.cancel();
    controller_task.await.unwrap().unwrap();
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
async fn saving_preserves_active_images_until_deploy_and_rename_preserves_resources() {
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let mut input = manifest();
    let first = harness.apply(input.clone(), Some(0)).await;
    harness.finish(&first).await;
    let pulls = harness.pulls().await;
    let piqueld_core::Source::Image { image } = &input.spec.services[0].source else {
        panic!("expected image fixture")
    };
    harness
        .docker
        .registry
        .lock()
        .await
        .digests
        .insert(image.to_string(), "b".repeat(64));
    input.spec.services[0].replicas = 2.into();
    input.spec.services[0]
        .environment
        .insert("TOKEN".into(), "private-value".into());
    let changed = harness.save(input.clone(), Some(1)).await;
    assert!(changed.operation_id.is_none());
    assert_eq!(harness.pulls().await, pulls);
    let app = harness.store.get(&first.environment_id).await.unwrap();
    assert!(
        app.resolved.unwrap().services[0]
            .image
            .as_str()
            .ends_with(&"a".repeat(64))
    );
    let refreshed = applications
        .deploy(&first.environment_id, Some(2))
        .await
        .unwrap();
    harness.finish(&refreshed).await;
    let before = harness
        .store
        .get(&first.environment_id)
        .await
        .unwrap()
        .resolved
        .unwrap();
    assert!(before.services[0].image.as_str().ends_with(&"b".repeat(64)));
    let renamed = applications
        .accept(
            Daemon,
            piqueld::api::Mutation::Rename {
                id: ApplicationId::parse(first.environment_id.as_str()).unwrap(),
                name: "renamed".into(),
            },
            Some(2),
            false,
            Some("rename-resources"),
        )
        .await
        .unwrap();
    let piqueld::api::MutationResponse::Rename(renamed) = renamed else {
        panic!("rename response")
    };
    assert_eq!(renamed.generation, 3);
    let after = harness
        .store
        .get(&first.environment_id)
        .await
        .unwrap()
        .resolved
        .unwrap();
    assert_eq!(after.services, before.services);
    assert_eq!(after.networks, before.networks);
    assert_eq!(after.volumes, before.volumes);
    assert_eq!(
        harness
            .store
            .latest_operation_for_environment(&first.environment_id)
            .await
            .unwrap()
            .unwrap()
            .id,
        refreshed.id
    );
    let events = harness.store.events(None, None, 100).await.unwrap().items;
    assert!(events.iter().any(|event| event.kind == "resource_mutated"
        && event.phase.as_deref() == Some("ensure_service")
        && event.resource.as_deref() == Some(after.services[0].name.as_str())));
    assert!(
        !serde_json::to_string(&events)
            .unwrap()
            .contains("private-value")
    );
}

#[tokio::test]
async fn failed_preparation_preserves_active_repair_and_save_does_not_retry() {
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let mut input = manifest();
    let first = harness.apply(input.clone(), None).await;
    harness.finish(&first).await;
    input.spec.services[0].source = piqueld_core::Source::Image {
        image: "ghcr.io/example/unstable:1".into(),
    };
    harness.docker.arm_tag_flips(100).await;
    let failed = harness.apply(input.clone(), None).await;
    harness.scan().await;
    let failure = harness.operation(&failed.id).await;
    assert_eq!(failure.state, OperationState::Failed);
    assert_eq!(failure.phase.as_deref(), Some("resolving_image"));
    assert_eq!(failure.resource.as_deref(), Some("web"));
    let pulls = harness.pulls().await;
    let response = applications
        .accept(
            Daemon,
            piqueld::api::Mutation::save(
                input.validate_template().unwrap(),
                Some(first.environment_id.to_string()),
                false,
            ),
            Some(failed.generation),
            false,
            None,
        )
        .await
        .unwrap();
    let piqueld::api::MutationResponse::Saved(saved) = response else {
        panic!("saved configuration")
    };
    assert!(saved.operation_id.is_none());
    assert_eq!(harness.operation(&failed.id).await, failure);
    assert_eq!(harness.pulls().await, pulls);
    {
        let mut observed = harness.docker.observed.lock().await;
        observed.services[0].replicas = 9;
        observed.services[0].convergence = Convergence::Degraded;
    }
    harness.scan().await;
    assert_eq!(harness.docker.observed.lock().await.services[0].replicas, 1);
    let stored = harness.store.get(&first.environment_id).await.unwrap();
    assert_eq!(stored.environment.resolved_generation, Some(1));
    let events = harness.store.events(None, None, 100).await.unwrap().items;
    assert!(events.iter().any(|event| event.kind == "operation_failed"
        && event.error_code.as_deref() == Some("image_resolution_failed")
        && event.resource.as_deref() == Some("web")));
}

#[tokio::test]
async fn convergence_timeout_closes_the_waiting_action_as_failed() {
    let mut harness = ControllerHarness::new().await;
    harness
        .docker
        .stall_convergence
        .store(true, Ordering::SeqCst);
    // The deadline covers network, volume and service creation before the wait
    // starts. Leave headroom so a loaded runner still reaches the waiting action.
    harness.set_convergence_timeout(Duration::from_secs(3));
    let created = harness.create().await;
    harness.scan().await;
    let events = harness.operation_events(&created.id).await;
    assert!(
        events.iter().any(|event| event.kind == "action_failed"
            && event.error_code.as_deref() == Some("convergence_timeout")
            && event.duration_ms.is_some()),
        "{events:#?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| event.kind == "action_outcome_unknown")
    );
}

/// Deploys image services given as `(name, depends_on)`, where each `slow`
/// service stays updating for that many observations after its ensure.
/// Returns the operation's final state and Docker's rollout log.
async fn deploy_dependents(
    services: &[(&str, &[&str])],
    slow: &[(&str, usize)],
    convergence_timeout: Duration,
) -> (OperationState, Vec<String>) {
    use std::fmt::Write;
    let mut manifest = String::from(
        "api_version = \"piqueld.dev/v1alpha1\"\nkind = \"Application\"\n[metadata]\nname = \"notes\"\n",
    );
    for (name, depends_on) in services {
        write!(
            manifest,
            "[[spec.services]]\nname = \"{name}\"\ndepends_on = {depends_on:?}\n[spec.services.source]\ntype = \"image\"\nimage = \"{}\"\n",
            prebuilt::IMAGE
        )
        .unwrap();
    }
    let mut harness = ControllerHarness::new().await;
    harness.set_application(
        piqueld_core::parse_toml(&manifest)
            .unwrap()
            .normalize(ApplicationId::parse("app-dependents-01").unwrap()),
    );
    harness.docker.slow_services.lock().await.extend(
        slow.iter()
            .map(|(name, observations)| ((*name).to_owned(), *observations)),
    );
    harness.set_convergence_timeout(convergence_timeout);
    let created = harness.create().await;
    harness.scan().await;
    let state = harness.operation(&created.id).await.state;
    (state, harness.docker.rollout.lock().await.clone())
}

/// A service that disappears while the deployment waits for it fails the
/// attempt, which is retried, instead of waiting out the convergence timeout.
#[tokio::test]
async fn a_service_removed_while_waiting_for_it_fails_the_attempt() {
    let harness = ControllerHarness::new().await;
    harness
        .docker
        .stall_convergence
        .store(true, Ordering::SeqCst);
    let operation = harness.create().await;
    let remove = async {
        eventually(Duration::from_secs(10), POLL, async || {
            (harness.operation(&operation.id).await.phase.as_deref() == Some("wait_for_service"))
                .then_some(())
        })
        .await
        .expect("the deployment waits for its service");
        harness.docker.observed.lock().await.services.clear();
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(harness.scan(), remove)
    })
    .await
    .expect("the wait ends once its service is gone");
    let failed = harness.operation(&operation.id).await;
    assert_eq!(failed.state, OperationState::Failed);
    assert_eq!(failed.error_code.as_deref(), Some("docker_request_failed"));
}

#[tokio::test]
async fn dependents_roll_out_once_their_dependencies_converge() {
    // `worker` is independent and sorts after the dependent `server`.
    let rollout = deploy_dependents(
        &[("db", &[]), ("server", &["db"]), ("worker", &[])],
        &[("db", 3)],
        Duration::from_mins(2),
    )
    .await;
    assert_eq!(
        rollout,
        (
            OperationState::Succeeded,
            vec![
                "ensure db".to_owned(),
                "ensure worker".to_owned(),
                "converged db".to_owned(),
                "ensure server".to_owned(),
            ]
        )
    );
}

#[tokio::test]
async fn each_dependency_gets_the_full_convergence_timeout() {
    // Each link converges in about 1.75s of backed-off polls: well within
    // the 4s timeout alone, but the chain as a whole takes longer.
    let rollout = deploy_dependents(
        &[("db", &[]), ("api", &["db"]), ("web", &["api"])],
        &[("db", 5), ("api", 5), ("web", 5)],
        Duration::from_secs(4),
    )
    .await;
    assert_eq!(
        rollout,
        (
            OperationState::Succeeded,
            ["db", "api", "web"]
                .iter()
                .flat_map(|name| [format!("ensure {name}"), format!("converged {name}")])
                .collect()
        )
    );
}

#[tokio::test]
async fn git_deploy_prepares_before_rollout_and_supersedes_pending_requests() {
    use piqueld::api::{Mutation, MutationResponse};
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let mut input = manifest();
    let fixture = git_fixture::GitBuildFixture::new();
    input.spec.services[0].source = fixture.source.clone();
    let first = harness.apply(input.clone(), Some(0)).await;
    let deploy = || Mutation::deploy(first.environment_id.clone());
    let MutationResponse::Operation(replacement) = applications
        .accept(Daemon, deploy(), None, true, None)
        .await
        .unwrap()
    else {
        panic!("deployment")
    };
    assert_eq!(
        harness.operation(&first.id).await.state,
        OperationState::Superseded
    );
    let replacement = harness.operation(&replacement.operation_id).await;
    harness.finish(&replacement).await;
    let active = harness
        .store
        .get(&first.environment_id)
        .await
        .unwrap()
        .resolved
        .unwrap();
    assert!(
        matches!(&active.services[0].source, ResolvedSource::Git { commit, .. } if piqueld_core::manifest::valid_git_commit(commit))
    );
    let pulls = harness.pulls().await;
    let keyed = || {
        let actor = Daemon;
        applications.accept(actor, deploy(), None, true, Some("git-deploy"))
    };
    let MutationResponse::Operation(accepted) = keyed().await.unwrap() else {
        panic!("expected operation")
    };
    let MutationResponse::Operation(replay) = keyed().await.unwrap() else {
        panic!("expected operation")
    };
    assert_eq!(accepted.operation_id, replay.operation_id);
    let operation = harness.operation(&accepted.operation_id).await;
    harness.finish(&operation).await;
    assert!(harness.pulls().await > pulls);
    input.spec.services[0].source = fixture.failing_source();
    let failed = harness.apply(input, Some(1)).await;
    harness.scan().await;
    let failure = harness.operation(&failed.id).await;
    assert_eq!(failure.state, OperationState::Failed);
    assert_eq!(failure.phase.as_deref(), Some("building_git"));
    assert_eq!(failure.resource.as_deref(), Some("web"));
    assert_eq!(failure.error_code.as_deref(), Some("git_build_failed"));
    assert_eq!(
        harness
            .store
            .get(&first.environment_id)
            .await
            .unwrap()
            .resolved
            .unwrap(),
        active
    );
    harness
        .assert_git_build_history(&first.environment_id)
        .await;
}

#[tokio::test]
async fn mixed_sources_report_the_failing_source() {
    for git_fails in [false, true] {
        let harness = ControllerHarness::new().await;
        let fixture = git_fixture::GitBuildFixture::new();
        let mut input = manifest();
        let mut git_service = input.spec.services[0].clone();
        git_service.name = "git-service".into();
        if git_fails {
            git_service.source = fixture.failing_source();
        } else {
            git_service.source = fixture.source.clone();
            input.spec.services[0].source = piqueld_core::Source::Image {
                image: "ghcr.io/example/unstable:1".into(),
            };
            harness.docker.arm_tag_flips(100).await;
        }
        input.spec.services.push(git_service);
        let operation = harness.apply(input, Some(0)).await;
        harness.scan().await;
        let failure = harness.operation(&operation.id).await;
        assert_eq!(failure.state, OperationState::Failed);
        let (phase, service, code) = if git_fails {
            ("building_git", "git-service", "git_build_failed")
        } else {
            ("resolving_image", "web", "image_resolution_failed")
        };
        assert_eq!(failure.phase.as_deref(), Some(phase));
        assert_eq!(failure.resource.as_deref(), Some(service));
        assert_eq!(failure.error_code.as_deref(), Some(code));
    }
}

mod repository_deployments {
    use super::*;
    use piqueld::api::{Mutation, MutationResponse, PreviewMutation};
    use piqueld::store::StoreError;
    use piqueld_core::manifest::{
        GitRepository, ManifestRevision, RepositoryManifest, SourceRepository,
    };
    use piqueld_core::{EnvironmentName, TrackedBranch};
    use repository::Repository;

    impl Repository {
        /// The fixture manifest, connected to `path` in this repository.
        fn manifest(&self, path: &str) -> ApplicationManifest {
            let mut application = manifest();
            application.spec.manifest = Some(RepositoryManifest {
                repository: GitRepository {
                    url: self.path().display().to_string(),
                    branch: "main".into(),
                    commit: None,
                },
                path: path.into(),
                sync: piqueld_core::sync::RepositorySync::Off,
            });
            application
        }

        /// Writes `manifest` to `path`, without committing it.
        fn write(&self, path: &str, manifest: &ApplicationManifest) {
            std::fs::write(
                self.path().join(path),
                serde_json::to_vec(manifest).unwrap(),
            )
            .unwrap();
        }

        /// Commits `manifest` and a new `Dockerfile` on `feat/local` of a
        /// working copy, a branch this repository never has, and bundles it
        /// against `base`. Returns the snapshot commit and the bundle.
        fn snapshot(&self, manifest: &ApplicationManifest, base: &str) -> (String, Vec<u8>) {
            let work = self.working_copy();
            work.git(&["switch", "-qc", "feat/local"]);
            let manifest = serde_json::to_vec(manifest).unwrap();
            let commit = work.commit(&[
                ("app.json", &manifest),
                ("Dockerfile", b"FROM alpine:3.21\n"),
            ]);
            (commit, work.bundle("feat/local", Some(base)))
        }
    }

    /// Builds `Dockerfile` from the manifest's own repository and revision.
    fn self_source() -> piqueld_core::Source {
        piqueld_core::Source::Git {
            repository: SourceRepository::Manifest(
                piqueld_core::manifest::ManifestRepository::Manifest,
            ),
            build: git_fixture::dockerfile_build(),
        }
    }

    impl ControllerHarness {
        /// Deploys `id`, fetching its tracked manifest, and runs the controller.
        async fn deploy(&self, id: &EnvironmentId) -> Operation {
            self.deploy_revision(id, None).await
        }

        /// Deploys `id` at `revision`, or what it tracks, and runs the controller.
        async fn deploy_revision(
            &self,
            id: &EnvironmentId,
            revision: Option<ManifestRevision>,
        ) -> Operation {
            let MutationResponse::Operation(accepted) = self
                .applications()
                .accept(
                    Daemon,
                    Mutation::Deploy {
                        id: id.clone(),
                        revision,
                    },
                    None,
                    true,
                    None,
                )
                .await
                .unwrap()
            else {
                panic!("operation expected")
            };
            self.scan().await;
            self.operation(&accepted.operation_id).await
        }

        /// The warnings recorded on `operation`'s deployment.
        async fn warnings(&self, operation: &Operation) -> Vec<String> {
            let deployments = self
                .store
                .deployments(&operation.environment_id, None, 3)
                .await
                .unwrap();
            deployments
                .items
                .into_iter()
                .filter(|deployment| deployment.operation.id == operation.id)
                .flat_map(|deployment| deployment.warnings)
                .map(|warning| warning.code)
                .collect()
        }

        /// Creates `name`, an environment of `production`'s application following `branch`.
        async fn create_environment(
            &self,
            production: &EnvironmentId,
            name: &str,
            branch: &str,
        ) -> EnvironmentId {
            let application = self.store.get(production).await.unwrap();
            let MutationResponse::Environment(environment) = self
                .applications()
                .accept(
                    Daemon,
                    Mutation::CreateEnvironment {
                        application: application.environment.application_id,
                        name: EnvironmentName::parse(name).unwrap(),
                        source: Some(piqueld::api::SourceChoice::Branch(
                            TrackedBranch::new(branch.into(), None).unwrap(),
                        )),
                    },
                    None,
                    true,
                    None,
                )
                .await
                .unwrap()
            else {
                panic!("environment expected")
            };
            environment.id
        }

        /// Runs `mutation` of a preview and the controller, and returns how its
        /// operation ended, unless a deletion removed it, with the preview.
        async fn run_preview(
            &self,
            mutation: Mutation,
        ) -> (Option<Operation>, Option<EnvironmentId>) {
            let (operation, preview) = match self
                .applications()
                .accept(Daemon, mutation, None, false, None)
                .await
                .unwrap()
            {
                MutationResponse::Preview(created) => (created.operation, Some(created.preview.id)),
                MutationResponse::Operation(operation) => (operation, None),
                _ => panic!("preview operation expected"),
            };
            self.scan().await;
            let operation = self.store.operation(&operation.operation_id).await.ok();
            (operation, preview)
        }
    }

    /// Repository files are read at the commit each deployment fetches, mount
    /// their bytes verbatim, and a missing one fails the fetch naming it.
    /// Saved manifests keep the path, never the bytes.
    #[tokio::test]
    async fn repository_files_are_captured_at_the_fetched_commit() {
        use piqueld_core::manifest::{FileMode, FileSource, ServiceFile};
        let repository = Repository::new();
        let harness = ControllerHarness::new().await;
        let mut main = repository.manifest("app.json");
        main.spec.services[0].files = vec![ServiceFile {
            target: "/etc/otel.yaml".into(),
            source: FileSource::Path("otel.yaml".into()),
            mode: FileMode::default(),
        }];
        repository.write("app.json", &main);
        let otel = repository.path().join("otel.yaml");
        std::fs::write(&otel, "home: ${HOME}\n").unwrap();
        repository.commit(&[]);
        let first = harness.apply(main.clone(), Some(0)).await;
        harness.finish(&first).await;
        let environment = first.environment_id;
        let deployed = async || {
            let current = harness.store.get(&environment).await.unwrap();
            assert!(current.application.application.files().is_empty());
            current.resolved.unwrap().services[0].files[0]
                .content
                .clone()
        };
        assert_eq!(deployed().await, "home: ${HOME}\n");

        std::fs::write(&otel, "home: /root\n").unwrap();
        repository.commit(&[]);
        let changed = harness.deploy(&environment).await;
        assert_eq!(changed.state, OperationState::Succeeded);
        assert_eq!(deployed().await, "home: /root\n");

        std::fs::remove_file(&otel).unwrap();
        repository.commit(&[]);
        let missing = harness.deploy(&environment).await;
        assert_eq!(
            (missing.state, missing.error_code.as_deref()),
            (OperationState::Failed, Some("manifest_invalid"))
        );
        assert_eq!(
            missing.error_message.as_deref(),
            Some("repository file otel.yaml does not exist at the fetched commit")
        );
        assert_eq!(deployed().await, "home: /root\n");
    }

    /// A fetched file's `spec.manifest` never redirects later fetches: it is
    /// ignored, with a warning when it names another repository or path, and
    /// may be omitted.
    #[tokio::test]
    async fn fetched_connections_are_ignored_with_a_warning() {
        let repository = Repository::new();
        let harness = ControllerHarness::new().await;
        let initial = repository.manifest("app.json");
        let mut redirected = initial.clone();
        let declared = redirected.spec.manifest.as_mut().unwrap();
        declared.path = "next.json".into();
        declared.repository.url = "/elsewhere".into();
        declared.repository.branch = "release".into();
        redirected.spec.services[0].replicas = 2.into();
        repository.write("app.json", &redirected);
        repository.commit(&[]);
        let first = harness.apply(initial.clone(), Some(0)).await;
        harness.finish(&first).await;
        assert_eq!(
            harness.warnings(&first).await,
            ["manifest_connection_ignored"]
        );
        let current = harness.store.get(&first.environment_id).await.unwrap();
        assert_eq!(current.resolved.as_ref().unwrap().services[0].replicas, 2);
        assert_eq!(
            current.tracking().unwrap().repository(),
            initial.spec.manifest
        );
        assert_eq!(
            current.application.application.spec().manifest,
            initial.spec.manifest
        );
        // Only the branch differs, as it does between environments: no warning.
        let mut branched = initial.clone();
        branched.spec.manifest.as_mut().unwrap().repository.branch = "release".into();
        // Without `spec.manifest`, the application stays connected, and
        // references that need it still validate.
        let mut bare = initial.clone();
        bare.spec.manifest = None;
        bare.spec.services[0]
            .environment
            .insert("REVISION".into(), "${{ git.sha }}".into());
        for file in [branched, bare] {
            repository.write("app.json", &file);
            repository.commit(&[]);
            let deployed = harness.deploy(&first.environment_id).await;
            assert_eq!(deployed.state, OperationState::Succeeded);
            assert_eq!(harness.warnings(&deployed).await, Vec::<String>::new());
            let current = harness.store.get(&first.environment_id).await.unwrap();
            assert_eq!(
                current.tracking().unwrap().repository(),
                initial.spec.manifest
            );
        }
    }

    /// Every deployment of a branch records its head for sync to follow from.
    #[tokio::test]
    async fn deployments_record_the_branch_head_sync_follows_from() {
        let repository = Repository::new();
        let harness = ControllerHarness::new().await;
        let main = repository.manifest("app.json");
        repository.write("app.json", &main);
        let first = repository.commit(&[]);
        let production = harness.apply(main.clone(), Some(0)).await;
        harness.finish(&production).await;
        let production = production.environment_id;
        let synced = async || {
            harness
                .store
                .get(&production)
                .await
                .unwrap()
                .environment
                .synced
                .and_then(|head| head.commit)
        };
        // A deployment of its branch records the head it fetched.
        assert_eq!(synced().await.as_deref(), Some(first.as_str()));
        // A one-off deployment of an older commit records the branch's head
        // then, so it stays until the branch moves.
        let second = repository.commit(&[]);
        let deployed = harness
            .deploy_revision(&production, Some(ManifestRevision::Commit(first.clone())))
            .await;
        assert_eq!(deployed.state, OperationState::Succeeded);
        assert_eq!(synced().await.as_deref(), Some(second.as_str()));
    }

    /// Environments following different branches each deploy their own
    /// branch, and a fetch for one never changes what the other deploys.
    #[tokio::test]
    async fn environments_on_different_branches_deploy_independently() {
        let repository = Repository::new();
        let harness = ControllerHarness::new().await;
        let mut main = repository.manifest("app.json");
        repository.write("app.json", &main);
        repository.commit(&[]);
        repository.git(&["checkout", "-b", "release"]);
        let mut release = main.clone();
        release.spec.services[0].replicas = 3.into();
        repository.write("app.json", &release);
        repository.commit(&[]);
        repository.git(&["checkout", "main"]);
        let production = harness.apply(main.clone(), Some(0)).await;
        harness.finish(&production).await;
        let production = production.environment_id;
        let staging = harness
            .create_environment(&production, "staging", "release")
            .await;
        let replicas = async |environment: &EnvironmentId| {
            let current = harness.store.get(environment).await.unwrap();
            (
                current
                    .resolved
                    .map(|resolved| resolved.services[0].replicas),
                current.own,
            )
        };
        let deployed = harness.deploy(&staging).await;
        assert_eq!(deployed.state, OperationState::Succeeded);
        let (staged, fetched) = replicas(&staging).await;
        assert_eq!(staged, Some(3));
        let (before, production_manifest) = replicas(&production).await;
        assert_eq!(before, Some(1));
        // Only a manifest fetched from the branch `spec.manifest` names
        // becomes the application's saved one.
        let saved_replicas = async || {
            let current = harness.store.get(&production).await.unwrap();
            current.application.application.spec().services[0]
                .replicas
                .clone()
        };
        assert_eq!(saved_replicas().await, main.spec.services[0].replicas);
        let deployed = harness.deploy(&production).await;
        assert_eq!(deployed.state, OperationState::Succeeded);
        assert_eq!(saved_replicas().await, main.spec.services[0].replicas);
        // A new main commit reaches production only, when it deploys.
        main.spec.services[0].replicas = 2.into();
        repository.write("app.json", &main);
        repository.commit(&[]);
        let deployed = harness.deploy(&production).await;
        assert_eq!(deployed.state, OperationState::Succeeded);
        assert_eq!(replicas(&production).await.0, Some(2));
        assert_ne!(replicas(&production).await.1, production_manifest);
        assert_eq!(replicas(&staging).await, (Some(3), fetched));
        // Pointing staging at main fetches main from its next deployment.
        harness
            .applications()
            .accept(
                Daemon,
                Mutation::SetBranch {
                    id: staging.clone(),
                    branch: TrackedBranch::new("main".into(), None).unwrap(),
                },
                None,
                true,
                None,
            )
            .await
            .unwrap();
        assert_eq!(replicas(&staging).await.0, Some(3));
        let deployed = harness.deploy(&staging).await;
        assert_eq!(deployed.state, OperationState::Succeeded);
        assert_eq!(replicas(&staging).await.0, Some(2));
        // One-off revisions deploy another branch without changing staging's.
        let deployed = harness
            .deploy_revision(&staging, Some(ManifestRevision::Branch("release".into())))
            .await;
        assert_eq!(deployed.state, OperationState::Succeeded);
        assert_eq!(replicas(&staging).await.0, Some(3));
        let current = harness.store.get(&staging).await.unwrap();
        assert_eq!(
            current.environment.source,
            piqueld_core::EnvironmentSource::Branch(
                TrackedBranch::new("main".into(), None).unwrap()
            )
        );
    }

    /// Deleting a preview removes every volume it ever created, including one
    /// its branch's manifest later dropped, while environments keep theirs.
    /// Unlike environments, it records no release.
    #[tokio::test]
    async fn deleting_a_preview_removes_every_volume_it_ever_created() {
        let repository = Repository::new();
        let harness = ControllerHarness::new().await;
        let main = repository.manifest("app.json");
        repository.write("app.json", &main);
        repository.commit(&[]);
        let production = harness.apply(main.clone(), Some(0)).await;
        harness.finish(&production).await;
        repository.git(&["checkout", "-b", "feat/cache"]);
        let mut cached = main.clone();
        cached
            .spec
            .volumes
            .push(serde_json::from_value(serde_json::json!({"name": "cache"})).unwrap());
        cached.spec.services[0].mounts.push(
            serde_json::from_value(
                serde_json::json!({"volume": "cache", "target": "/var/cache/notes"}),
            )
            .unwrap(),
        );
        repository.write("app.json", &cached);
        repository.commit(&[]);
        let volumes = async || {
            let observed = harness.docker.observed.lock().await;
            observed
                .volumes
                .iter()
                .map(|volume| volume.name.clone())
                .collect::<Vec<_>>()
        };
        let before = volumes().await;

        let (created, preview) = harness
            .run_preview(Mutation::Preview(PreviewMutation::Create {
                application: harness
                    .store
                    .get(&production.environment_id)
                    .await
                    .unwrap()
                    .environment
                    .application_id,
                branch: "feat/cache".parse().unwrap(),
                slot: None,
            }))
            .await;
        assert_eq!(created.unwrap().state, OperationState::Succeeded);
        let preview = preview.unwrap();
        // Previews never record releases; the environment that deployed did.
        assert_eq!(harness.store.current_release(&preview).await.unwrap(), None);
        assert!(
            harness
                .store
                .current_release(&production.environment_id)
                .await
                .unwrap()
                .is_some()
        );
        // The branch drops the cache volume; the preview keeps it until deleted.
        repository.write("app.json", &main);
        repository.commit(&[]);
        let (deployed, _) = harness
            .run_preview(Mutation::Preview(PreviewMutation::Deploy {
                id: preview.clone(),
            }))
            .await;
        assert_eq!(deployed.unwrap().state, OperationState::Succeeded);
        let inventory = harness.store.preview_volumes(&preview).await.unwrap();
        assert_eq!(inventory.len(), 2);
        assert!(inventory.iter().all(|name| !before.contains(name)));
        let current = volumes().await;
        assert!(inventory.iter().all(|name| current.contains(name)));

        let (deleted, _) = harness
            .run_preview(Mutation::Preview(PreviewMutation::Delete {
                id: preview.clone(),
            }))
            .await;
        assert!(deleted.is_none(), "{deleted:?}");
        assert!(matches!(
            harness.store.get(&preview).await,
            Err(StoreError::NotFound)
        ));
        let remaining = volumes().await;
        assert!(inventory.iter().all(|name| !remaining.contains(name)));
        assert_eq!(remaining, before, "production keeps its volume");
    }

    #[tokio::test]
    async fn missing_invalid_and_failed_build_manifests_preserve_accepted_configuration() {
        let repository = Repository::new();
        repository.commit(&[]);
        let harness = ControllerHarness::new().await;
        let initial = repository.manifest("app.json");
        repository.write("app.json", &initial);
        repository.commit(&[]);
        let first = harness.apply(initial.clone(), Some(0)).await;
        harness.finish(&first).await;
        let before = harness.store.get(&first.environment_id).await.unwrap();
        std::fs::remove_file(repository.path().join("app.json")).unwrap();
        repository.commit(&[]);
        let missing = harness.deploy(&first.environment_id).await;
        assert_eq!(missing.error_code.as_deref(), Some("manifest_not_found"));
        let mut invalid = initial.clone();
        invalid.metadata.name = "different-application".into();
        repository.write("app.json", &invalid);
        repository.commit(&[]);
        let invalid = harness.deploy(&first.environment_id).await;
        assert_eq!(invalid.error_code.as_deref(), Some("manifest_invalid"));
        let build = git_fixture::GitBuildFixture::new();
        let mut failing = initial.clone();
        failing.spec.manifest = None;
        failing.spec.services[0].source = build.failing_source();
        repository.write("app.json", &failing);
        repository.commit(&[]);
        let failed = harness.deploy(&first.environment_id).await;
        assert_eq!(failed.error_code.as_deref(), Some("git_build_failed"));
        let after = harness.store.get(&first.environment_id).await.unwrap();
        assert_eq!(before.application, after.application);
        assert_eq!(before.application.generation, after.application.generation);
        assert_eq!(before.resolved, after.resolved);
        assert_eq!(harness.store.list(None, 50).await.unwrap().items.len(), 1);
    }

    /// The connection's branch deploys the saved manifest, edits included,
    /// until a push changes the manifest there, which replaces it.
    #[tokio::test]
    async fn edits_deploy_until_a_push_changes_the_manifest() {
        let repository = Repository::new();
        let harness = ControllerHarness::new().await;
        let initial = repository.manifest("app.json");
        repository.write("app.json", &initial);
        repository.commit(&[]);
        let first = harness.apply(initial.clone(), Some(0)).await;
        harness.finish(&first).await;
        let environment = first.environment_id.clone();
        let saved = || async { harness.store.get(&environment).await.unwrap() };
        let id = saved().await.application.application.id().clone();
        let generation = saved().await.application.generation;
        let mut edited = initial.clone();
        edited.spec.services[0].replicas = 4.into();
        harness.save(edited, Some(generation)).await;
        // A push that leaves the manifest alone deploys the edit.
        repository.commit(&[("README.md", b"notes")]);
        harness.finish(&harness.deploy(&environment).await).await;
        let current = saved().await;
        assert_eq!(current.own.unwrap().spec().services[0].replicas, 4.into());
        let view = harness.applications().application(&id).await.unwrap();
        assert!(view.edited());
        assert_eq!(
            view.edits()
                .into_iter()
                .map(|edit| edit.field)
                .collect::<Vec<_>>(),
            ["services.web.replicas"]
        );
        // One that changes it replaces the saved manifest.
        let mut pushed = initial;
        pushed.spec.services[0]
            .environment
            .insert("MODE".into(), "pushed".into());
        repository.write("app.json", &pushed);
        repository.commit(&[]);
        harness.finish(&harness.deploy(&environment).await).await;
        let current = saved().await;
        for deployed in [
            current.own.as_ref().unwrap(),
            &current.application.application,
        ] {
            let web = &deployed.spec().services[0];
            assert_eq!(web.replicas, 1.into());
            assert_eq!(web.environment["MODE"], "pushed");
        }
        assert_eq!(
            current.environment.resolved_generation,
            Some(current.application.generation)
        );
        let view = harness.applications().application(&id).await.unwrap();
        assert!(!view.edited());
    }

    #[tokio::test]
    async fn retry_after_reopening_store_uses_fetched_manifest_and_new_deploy_fetches_again() {
        let repository = Repository::new();
        let mut harness = ControllerHarness::new().await;
        let initial = repository.manifest("app.json");
        repository.write("app.json", &initial);
        repository.commit(&[]);
        let first = harness.apply(initial.clone(), Some(0)).await;
        harness.finish(&first).await;
        let build = git_fixture::GitBuildFixture::new();
        let mut candidate = initial.clone();
        candidate.spec.services[0].source = build.failing_source();
        repository.write("app.json", &candidate);
        repository.commit(&[]);
        let failed = harness.deploy(&first.environment_id).await;
        assert_eq!(failed.error_code.as_deref(), Some("git_build_failed"));
        repository.write("app.json", &initial);
        repository.commit(&[]);
        harness.reopen().await;
        let retry = harness
            .applications()
            .reconcile(&first.environment_id, None)
            .await
            .unwrap();
        assert_eq!(retry.id, failed.id);
        harness.scan().await;
        assert_eq!(
            harness.operation(&retry.id).await.state,
            OperationState::Failed
        );
        assert_eq!(
            harness.deploy(&first.environment_id).await.state,
            OperationState::Succeeded
        );
    }

    #[tokio::test]
    async fn self_sources_build_the_manifest_commit_including_one_time_revisions() {
        let repository = Repository::new();
        let harness = ControllerHarness::new().await;
        let mut fetched = repository.manifest("app.json");
        fetched.spec.services[0].source = self_source();
        repository.write("app.json", &fetched);
        std::fs::write(repository.path().join("Dockerfile"), "FROM alpine:3.20\n").unwrap();
        let main = repository.commit(&[]);
        repository.git(&["checkout", "-b", "feature"]);
        std::fs::write(repository.path().join("Dockerfile"), "FROM alpine:3.21\n").unwrap();
        let feature = repository.commit(&[]);
        repository.git(&["checkout", "main"]);
        let mut bootstrap = repository.manifest("app.json");
        bootstrap.spec.services.clear();
        let saved = harness.save(bootstrap, Some(0)).await;
        let application_id = EnvironmentId::parse(saved.application_id).unwrap();
        // The override is not saved, and saved configuration keeps "self":
        // only the fetch of `main` itself becomes the saved manifest.
        let expected_spec = fetched
            .clone()
            .validate()
            .unwrap()
            .normalize(ApplicationId::parse(application_id.as_str()).unwrap())
            .to_manifest()
            .spec;
        let mut saved_main = false;
        for (revision, expected) in [
            (
                Some(ManifestRevision::Commit(feature.clone())),
                feature.clone(),
            ),
            (None, main),
            (Some(ManifestRevision::Branch("feature".into())), feature),
        ] {
            let one_off = revision.clone();
            let deployment = harness.deploy_revision(&application_id, revision).await;
            assert_eq!(deployment.state, OperationState::Succeeded);
            let stored = harness.store.get(&application_id).await.unwrap();
            assert!(
                matches!(&stored.resolved.as_ref().unwrap().services[0].source, ResolvedSource::Git { commit, .. } if commit == &expected)
            );
            saved_main |= one_off.is_none();
            assert_eq!(
                stored.application.application.to_manifest().spec == expected_spec,
                saved_main
            );
        }
        // "self" builds the repository the manifest was fetched from, whatever
        // the file's own `spec.manifest` says.
        let mut moved = fetched;
        moved.spec.manifest.as_mut().unwrap().repository.url = "/elsewhere".into();
        repository.write("app.json", &moved);
        let head = repository.commit(&[]);
        let deployment = harness.deploy(&application_id).await;
        assert_eq!(deployment.state, OperationState::Succeeded);
        let stored = harness.store.get(&application_id).await.unwrap();
        assert!(
            matches!(&stored.resolved.as_ref().unwrap().services[0].source, ResolvedSource::Git { commit, .. } if commit == &head)
        );
    }

    /// A local snapshot of a never-pushed branch, uploaded thin, deploys its
    /// own manifest and builds `"self"` sources from the snapshot, through
    /// the preview's normal deployment path. Its history records where it
    /// came from, and sync records the branch as absent.
    #[tokio::test]
    async fn local_snapshots_deploy_their_manifest_and_build_self_sources_from_the_snapshot() {
        let repository = Repository::new();
        let harness = ControllerHarness::new().await;
        let mut fetched = repository.manifest("app.json");
        fetched.spec.services[0].source = self_source();
        repository.write("app.json", &fetched);
        std::fs::write(repository.path().join("Dockerfile"), "FROM alpine:3.20\n").unwrap();
        let base = repository.commit(&[]);
        let mut bootstrap = repository.manifest("app.json");
        bootstrap.spec.services.clear();
        let saved = harness.save(bootstrap, Some(0)).await;
        let application = ApplicationId::parse(saved.application_id).unwrap();

        // The snapshot changes the manifest and the Dockerfile on a branch
        // the repository never had.
        let mut local = fetched.clone();
        local.spec.services[0]
            .environment
            .insert("SNAPSHOT".into(), "yes".into());
        let (snapshot, bundle) = repository.snapshot(&local, &base);
        let created = harness
            .applications()
            .deploy_snapshot(
                Daemon,
                (
                    application,
                    piqueld_core::GitBranch::parse("feat/local").unwrap(),
                    None,
                ),
                (
                    futures_util::stream::once(async {
                        Ok::<_, std::io::Error>(axum::body::Bytes::from(bundle))
                    }),
                    None,
                ),
                None,
            )
            .await
            .unwrap();
        let MutationResponse::Preview(created) = created else {
            panic!("preview expected")
        };
        harness.scan().await;
        let operation = harness.operation(&created.operation.operation_id).await;
        assert_eq!(operation.state, OperationState::Succeeded);
        let stored = harness.store.get(&created.preview.id).await.unwrap();
        let service = &stored.resolved.as_ref().unwrap().services[0];
        assert!(
            matches!(&service.source, ResolvedSource::Git { commit, .. } if commit == &snapshot)
        );
        assert_eq!(
            service.environment.get("SNAPSHOT").map(String::as_str),
            Some("yes")
        );
        let history = harness
            .store
            .deployments(&created.preview.id, None, 3)
            .await
            .unwrap();
        assert_eq!(
            history.items[0].origin,
            piqueld_core::api::DeploymentOrigin::Local(piqueld_core::api::LocalSnapshot {
                commit: snapshot.clone(),
                base: Some(base),
            })
        );
        assert_eq!(
            history.items[0].system_value(piqueld_core::manifest::SystemVariable::GitSha),
            Some(snapshot.as_str())
        );
        let synced = stored.environment.synced.unwrap();
        assert_eq!(synced.commit, None);
    }

    #[tokio::test]
    async fn retry_after_reopening_store_builds_self_sources_at_the_fetched_commit() {
        let repository = Repository::new();
        let mut harness = ControllerHarness::new().await;
        repository.commit(&[]);
        repository.git(&["checkout", "-b", "feature"]);
        let mut fetched = repository.manifest("app.json");
        let mut built = fetched.spec.services[0].clone();
        built.name = "built".into();
        built.source = self_source();
        fetched.spec.services.push(built);
        repository.write("app.json", &fetched);
        let dockerfile = repository.path().join("Dockerfile");
        std::fs::write(&dockerfile, "FROM alpine:3.20\n").unwrap();
        let pinned = repository.commit(&[]);
        let mut bootstrap = repository.manifest("app.json");
        bootstrap.spec.services.clear();
        let saved = harness.save(bootstrap, Some(0)).await;
        let application_id = EnvironmentId::parse(saved.application_id).unwrap();
        // The image service fails preparation after the manifest is fetched.
        harness.docker.arm_tag_flips(100).await;
        let failed = harness
            .deploy_revision(
                &application_id,
                Some(ManifestRevision::Branch("feature".into())),
            )
            .await;
        assert_eq!(
            failed.error_code.as_deref(),
            Some("image_resolution_failed")
        );
        std::fs::write(&dockerfile, "FROM alpine:3.21\n").unwrap();
        repository.commit(&[]);
        harness.reopen().await;
        harness.docker.arm_tag_flips(0).await;
        let retry = harness
            .applications()
            .reconcile(&application_id, None)
            .await
            .unwrap();
        assert_eq!(retry.id, failed.id);
        harness.finish(&retry).await;
        let resolved = harness.target(&application_id).await;
        let commit = resolved
            .services
            .iter()
            .find_map(|service| match &service.source {
                ResolvedSource::Git { commit, .. } => Some(commit),
                ResolvedSource::Image { .. } => None,
            });
        assert_eq!(commit, Some(&pinned));
    }

    #[tokio::test]
    async fn bootstrap_can_pin_manifest_revision_independently_from_git_build_revision() {
        let repository = Repository::new();
        let harness = ControllerHarness::new().await;
        let build_repository = Repository::new();
        std::fs::write(
            build_repository.path().join("Dockerfile"),
            "FROM alpine:3.20\n",
        )
        .unwrap();
        let build_commit = build_repository.commit(&[]);
        let mut fetched = repository.manifest("app.json");
        fetched.spec.services[0].source = piqueld_core::Source::Git {
            repository: piqueld_core::manifest::SourceRepository::Git(GitRepository {
                url: build_repository.path().display().to_string(),
                branch: "main".into(),
                commit: Some(build_commit.clone()),
            }),
            build: git_fixture::dockerfile_build(),
        };
        repository.write("app.json", &fetched);
        let pinned = repository.commit(&[]);
        let mut bootstrap = repository.manifest("app.json");
        bootstrap.spec.services.clear();
        bootstrap.spec.manifest.as_mut().unwrap().repository.commit = Some(pinned);
        fetched.metadata.name = "wrong-later-name".into();
        repository.write("app.json", &fetched);
        repository.commit(&[]);
        let saved = harness.save(bootstrap, Some(0)).await;
        let application_id = EnvironmentId::parse(saved.application_id).unwrap();
        let deployment = harness.deploy(&application_id).await;
        assert_eq!(deployment.state, OperationState::Succeeded);
        let resolved = harness
            .store
            .get(&application_id)
            .await
            .unwrap()
            .resolved
            .unwrap();
        assert!(
            matches!(&resolved.services[0].source, ResolvedSource::Git { commit, .. } if commit == &build_commit)
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
    let (application_id, operation_id) = {
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
        (harness.application.id().to_string(), operation.id)
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
    assert_eq!(completed["span"]["environment_id"], application_id);
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
                    environment_id: Some(harness.application.id().to_string()),
                    kind: Some("diagnostic".into()),
                    error_code: Some("docker_unavailable".into()),
                    ..Default::default()
                },
                &piqueld::store::Visibility::ALL,
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

#[tokio::test]
async fn secret_rotation_requires_deploy_and_service_references_follow_pinned_versions() {
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let first = harness.apply(manifest(), Some(0)).await;
    harness.finish(&first).await;
    let app = piqueld_core::ApplicationId::parse(first.environment_id.as_str()).unwrap();
    harness
        .store_token(&first.environment_id, 0, b"version-one")
        .await;
    let deployment = harness.apply(with_token(manifest()), Some(1)).await;
    harness.finish(&deployment).await;
    let target = harness.target(&first.environment_id).await;
    let old = target.secret_names["token"].clone();
    assert_eq!(
        harness.docker.secrets.lock().await[&old].value,
        b"version-one"
    );
    harness
        .store_token(&first.environment_id, 1, b"version-two")
        .await;
    harness.scan().await;
    assert_eq!(
        harness.target(&first.environment_id).await,
        target,
        "rotation alone must not update services"
    );
    let next = applications
        .deploy(&first.environment_id, None)
        .await
        .unwrap();
    harness.finish(&next).await;
    let target = harness.target(&first.environment_id).await;
    let new = &target.secret_names["token"];
    assert_ne!(&old, new);
    assert_eq!(
        harness.docker.secrets.lock().await[new].value,
        b"version-two"
    );
    assert_eq!(
        harness
            .docker
            .observe(&first.environment_id)
            .await
            .unwrap()
            .services[0]
            .secrets,
        target.services[0].secrets
    );

    // Historical completed deployments must not block deletion forever.
    let clean = harness.apply(manifest(), Some(2)).await;
    harness.finish(&clean).await;
    applications
        .delete_stored_secret(Daemon, &app, "token", 2)
        .await
        .unwrap();
    assert!(harness.docker.secrets.lock().await.is_empty());
    assert!(harness.store.stored_secrets(&app).await.unwrap().is_empty());
}

#[tokio::test]
async fn application_deletion_journals_secret_cleanup_failures() {
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let first = harness.apply(manifest(), Some(0)).await;
    harness.finish(&first).await;
    harness
        .store
        .put_secret(Daemon, &first.environment_id, "token", 0, b"value".to_vec())
        .await
        .unwrap();
    harness
        .docker
        .fail_secret_removal
        .store(true, Ordering::SeqCst);
    let deletion = applications
        .delete(&first.environment_id, Some(1))
        .await
        .unwrap();
    harness.scan().await;
    let events = harness.operation_events(&deletion.id).await;
    let phases = |kind: &str| {
        events
            .iter()
            .filter(|event| event.kind == kind && event.phase.as_deref() == Some("remove_secrets"))
            .count()
    };
    assert_eq!(phases("action_requested"), 1, "{events:#?}");
    assert!(events.iter().any(|event| event.kind == "action_failed"
        && event.phase.as_deref() == Some("remove_secrets")
        && event.error_code.as_deref() == Some("docker_request_failed")));
    assert!(harness.store.get(&first.environment_id).await.is_ok());
}

/// A mounted secret nobody stored fails the deployment as `secret_missing`,
/// naming it, rather than as a journal failure.
#[tokio::test]
async fn missing_stored_secrets_fail_deployment_with_their_names() {
    let harness = ControllerHarness::new().await;
    let failed = harness.apply(with_token(manifest()), Some(0)).await;
    harness.scan().await;
    let result = harness.operation(&failed.id).await;
    assert_eq!(result.error_code.as_deref(), Some("secret_missing"));
    assert!(result.error_message.as_deref().unwrap().contains("token"));
}

#[tokio::test]
async fn secret_key_recovery_leaves_running_services_and_blocks_discarded_rollouts() {
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let initial = harness.apply(manifest(), Some(0)).await;
    harness.finish(&initial).await;
    let id = &initial.environment_id;
    harness.store_token(id, 0, b"original").await;
    let deployed = harness.apply(with_token(manifest()), Some(1)).await;
    harness.finish(&deployed).await;
    let target = harness.target(id).await;
    let original = harness.docker.observe(id).await.unwrap();

    std::fs::remove_file(harness.database_path.with_file_name("secrets.key")).unwrap();
    harness.store.recover_secret_key(Daemon).await.unwrap();
    let failed = applications.deploy(id, None).await.unwrap();
    harness.scan().await;
    let result = harness.operation(&failed.id).await;
    assert_eq!(result.error_code.as_deref(), Some("secret_unavailable"));
    assert!(result.error_message.as_deref().unwrap().contains("token"));
    assert_eq!(harness.target(id).await, target);
    assert_eq!(harness.docker.observe(id).await.unwrap(), original);

    harness.store_token(id, 1, b"replacement").await;
    let replacement = applications.deploy(id, None).await.unwrap();
    harness.finish(&replacement).await;
    let new_target = harness.target(id).await;
    assert_eq!(
        harness.docker.secrets.lock().await[&new_target.secret_names["token"]].value,
        b"replacement"
    );
    assert_eq!(
        harness.docker.secrets.lock().await[&target.secret_names["token"]].value,
        b"original",
        "running services keep their existing Docker secret"
    );
}

#[tokio::test]
async fn missing_secret_key_fails_rollout_before_docker_mutation() {
    let harness = ControllerHarness::new().await;
    let first = harness.apply(manifest(), Some(0)).await;
    harness.finish(&first).await;
    harness
        .store_token(&first.environment_id, 0, b"value")
        .await;
    std::fs::remove_file(harness.database_path.with_file_name("secrets.key")).unwrap();
    let deployment = harness.apply(with_token(manifest()), Some(1)).await;
    harness.scan().await;
    let operation = harness.operation(&deployment.id).await;
    assert_eq!(
        operation.error_code.as_deref(),
        Some("secret_storage_unavailable")
    );
    assert!(harness.docker.secrets.lock().await.is_empty());
    let events = harness.operation_events(&deployment.id).await;
    let service = |kind: &str| {
        events
            .iter()
            .find(|event| event.kind == kind && event.phase.as_deref() == Some("ensure_service"))
    };
    let failed = service("action_failed").expect("service action failed");
    assert_eq!(
        failed.error_code.as_deref(),
        Some("secret_storage_unavailable")
    );
    assert_eq!(
        failed.diagnostic.as_ref().unwrap().causes,
        ["Secret master key: missing"]
    );
    assert!(
        service("action_requested").is_none(),
        "no Docker request is made without secret values: {events:#?}"
    );
}

#[tokio::test]
async fn preparation_timeout_is_retried_after_backoff() {
    let gate = Arc::new(Gate::default());
    let mut harness = ControllerHarness::with_docker(FakeDocker {
        resolution_gate: Some(Arc::clone(&gate)),
        ..FakeDocker::default()
    })
    .await;
    harness.controller = Controller::new(Arc::clone(&harness.docker), Arc::clone(&harness.store))
        .with_prepare_timeout(Duration::from_millis(100));
    let mut manifest = manifest();
    manifest.spec.services[0].source = piqueld_core::Source::Image {
        image: "ghcr.io/example/slow:1".into(),
    };
    let operation = harness.apply(manifest, None).await;
    harness.scan().await;
    let failed = harness.operation(&operation.id).await;
    assert_eq!(failed.state, OperationState::Failed);
    assert_eq!(failed.error_code.as_deref(), Some("preparation_timeout"));
    assert!(
        harness
            .store
            .prepared_target(&operation.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        harness.docker.observed.lock().await.services,
        [] as [ObservedService; 0]
    );
    harness.expire_backoff(&operation).await;
    harness.controller = Controller::new(Arc::clone(&harness.docker), Arc::clone(&harness.store));
    gate.release.notify_one();
    harness.scan().await;
    let latest = harness
        .store
        .latest_operation_for_environment(&operation.environment_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.state, OperationState::Succeeded);
    assert_eq!(harness.docker.observed.lock().await.services.len(), 1);
}

#[tokio::test]
async fn changed_swarm_topology_blocks_preparation_and_recovers_after_backoff() {
    let harness = ControllerHarness::new().await;
    harness
        .docker
        .incompatible_swarm
        .store(true, Ordering::SeqCst);
    let operation = harness.apply(manifest(), None).await;
    harness.scan().await;
    assert_eq!(
        harness.operation(&operation.id).await.error_code.as_deref(),
        Some("swarm_topology_unsupported")
    );
    assert!(harness.docker.registry.lock().await.pulls.is_empty());
    assert_eq!(
        harness.docker.observed.lock().await.networks,
        [] as [ObservedNetwork; 0]
    );
    harness
        .docker
        .incompatible_swarm
        .store(false, Ordering::SeqCst);
    harness.expire_backoff(&operation).await;
    harness.scan().await;
    let latest = harness
        .store
        .latest_operation_for_environment(&operation.environment_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.state, OperationState::Succeeded);
}

#[tokio::test]
async fn topology_change_during_preparation_blocks_promotion_and_mutation() {
    let gate = Arc::new(Gate::default());
    let harness = ControllerHarness::with_docker(FakeDocker {
        resolution_gate: Some(Arc::clone(&gate)),
        ..FakeDocker::default()
    })
    .await;
    let mut manifest = manifest();
    manifest.spec.services[0].source = piqueld_core::Source::Image {
        image: "ghcr.io/example/slow:1".into(),
    };
    let operation = harness.apply(manifest, None).await;
    let cancellation = CancellationToken::new();
    let scan = harness.controller.scan(&cancellation);
    let change_topology = async {
        gate.entered.notified().await;
        harness
            .docker
            .incompatible_swarm
            .store(true, Ordering::SeqCst);
        gate.release.notify_one();
    };
    let (result, ()) = tokio::join!(scan, change_topology);
    result.unwrap();
    assert_eq!(
        harness.operation(&operation.id).await.error_code.as_deref(),
        Some("swarm_topology_unsupported")
    );
    assert!(
        harness
            .store
            .get(&operation.environment_id)
            .await
            .unwrap()
            .resolved
            .is_none()
    );
    assert_eq!(
        harness.docker.observed.lock().await.networks,
        [] as [ObservedNetwork; 0]
    );
}

#[tokio::test]
async fn deletion_proceeds_after_another_node_joins() {
    let harness = ControllerHarness::new().await;
    harness.create().await;
    harness.scan().await;
    harness
        .docker
        .incompatible_swarm
        .store(true, Ordering::SeqCst);
    harness.delete().await;
    harness.scan().await;
    harness.assert_deleted().await;
}

#[tokio::test]
async fn job_dependencies_converge_before_migration_and_other_services() {
    let mut harness = ControllerHarness::new().await;
    harness.set_convergence_timeout(Duration::from_secs(3));
    harness
        .docker
        .slow_services
        .lock()
        .await
        // Each dependency waits about 1.75s of backed-off polls, with room
        // for journal I/O. Their combined wait still exceeds the three-second
        // budget.
        .extend([("storage".into(), 5), ("db".into(), 5)]);
    let mut input = manifest_with_job_dependencies();
    // A prerequisite's own job must succeed before that service is started.
    let mut storage_job = input.spec.jobs[0].clone();
    storage_job.name = "initialize-storage".into();
    storage_job.service = "storage".into();
    input.spec.jobs.insert(0, storage_job);
    let operation = harness.apply(input, None).await;
    harness.finish(&operation).await;
    assert_eq!(
        *harness.docker.job_starts.lock().await,
        [
            JobStart {
                job: "initialize-storage".into(),
                services: 0,
                networks: 1,
                volumes: 1
            },
            JobStart {
                job: "migrate".into(),
                services: 2,
                networks: 1,
                volumes: 1
            },
        ]
    );
    let rollout = harness.docker.rollout.lock().await;
    assert_eq!(
        &rollout[..6],
        [
            "job initialize-storage",
            "ensure storage",
            "converged storage",
            "ensure db",
            "converged db",
            "job migrate"
        ]
    );
    assert!(rollout[6..].contains(&"ensure web".to_owned()));
    assert!(rollout[6..].contains(&"ensure other".to_owned()));
}

#[tokio::test]
async fn failed_or_timed_out_job_dependencies_prevent_jobs_and_other_services() {
    for failed in [false, true] {
        let mut harness = ControllerHarness::new().await;
        harness.set_convergence_timeout(Duration::from_secs(3));
        harness
            .docker
            .stall_convergence
            .store(!failed, Ordering::SeqCst);
        harness
            .docker
            .fail_convergence
            .store(failed, Ordering::SeqCst);
        let operation = harness.apply(manifest_with_job_dependencies(), None).await;
        assert_eq!(
            harness.scan_result(&operation).await,
            (
                OperationState::Failed,
                Some(
                    if failed {
                        "service_update_failed"
                    } else {
                        "convergence_timeout"
                    }
                    .into()
                )
            )
        );
        assert_eq!(harness.started_jobs().await, [] as [String; 0]);
        assert_eq!(*harness.docker.rollout.lock().await, ["ensure storage"]);
        assert!(
            harness
                .store
                .get(&operation.environment_id)
                .await
                .unwrap()
                .resolved
                .is_none()
        );
    }
}

#[tokio::test]
async fn failed_migration_rolls_back_prerequisites_and_repairs_active_services() {
    for existing_dependencies in [false, true] {
        let harness = ControllerHarness::new().await;
        let mut initial = manifest_with_job_dependencies();
        initial.spec.jobs.clear();
        if !existing_dependencies {
            initial.spec.services[0].depends_on.clear();
            initial
                .spec
                .services
                .retain(|service| matches!(service.name.as_str(), "web" | "other"));
        }
        let first = harness.apply(initial, None).await;
        harness.finish(&first).await;
        let mut replacement = manifest_with_job_dependencies();
        for service in &mut replacement.spec.services {
            if matches!(service.name.as_str(), "web" | "db") {
                service.replicas = 2.into();
            }
        }
        harness.docker.job_exit.store(3, Ordering::SeqCst);
        let operation = harness.apply(replacement, None).await;
        assert_eq!(
            harness.scan_result(&operation).await,
            (OperationState::Failed, Some("job_failed".into()))
        );
        let replicas = |observed: &ObservedApplication, name: &str| {
            observed
                .services
                .iter()
                .find(|service| service.labels[SERVICE_LABEL] == name)
                .map(|service| service.replicas)
        };
        // The job ran against the updated prerequisites.
        assert_eq!(
            replicas(&*harness.docker.observed.lock().await, "db"),
            Some(2)
        );
        {
            let mut observed = harness.docker.observed.lock().await;
            for service in &mut observed.services {
                if matches!(service.labels[SERVICE_LABEL].as_str(), "web" | "other") {
                    service.replicas = 9;
                }
            }
        }
        // The failed deployment ended, so repair returns prerequisites to
        // the active target, alongside the other active services.
        for _ in 0..8 {
            assert_eq!(
                harness.scan_result(&operation).await,
                (OperationState::Failed, Some("job_failed".into()))
            );
        }
        let observed = harness.docker.observed.lock().await;
        let previous = existing_dependencies.then_some(1);
        assert_eq!(replicas(&observed, "db"), previous);
        assert_eq!(replicas(&observed, "storage"), previous);
        assert_eq!(replicas(&observed, "web"), Some(1));
        assert_eq!(replicas(&observed, "other"), Some(1));
        assert_eq!(harness.started_jobs().await, ["migrate"]);
        assert_eq!(
            harness
                .store
                .get(&operation.environment_id)
                .await
                .unwrap()
                .environment
                .resolved_generation,
            Some(1)
        );
    }
}

/// A deployment whose own plan a conflict blocks has ended until an
/// operator resolves it, so repair returns its job prerequisites to the
/// active target. A conflict planning can't see is retried at once instead,
/// so its prerequisites stay rather than being rolled back and forth.
#[tokio::test]
async fn a_deployment_blocked_by_a_conflict_rolls_back_its_job_prerequisites() {
    let harness = ControllerHarness::new().await;
    let mut initial = manifest_with_job_dependencies();
    initial.spec.jobs.clear();
    initial.spec.services[0].depends_on.clear();
    initial
        .spec
        .services
        .retain(|service| matches!(service.name.as_str(), "web" | "other"));
    let first = harness.apply(initial, None).await;
    harness.finish(&first).await;
    let mut replacement = manifest_with_job_dependencies();
    let mut extra = replacement.spec.services[3].clone();
    extra.name = "extra".into();
    replacement.spec.services.push(extra);
    *harness.docker.conflicting_job.lock().await = Some("migrate".into());
    let operation = harness.apply(replacement, None).await;
    for _ in 0..3 {
        assert_eq!(
            harness.scan_result(&operation).await,
            (OperationState::Failed, Some("ownership_conflict".into()))
        );
        assert_eq!(
            harness.owned_services().await,
            ["db", "other", "storage", "web"]
        );
    }

    // A foreign service takes the name of the one the deployment adds.
    let foreign = harness.add_foreign_service(&operation, "extra").await;
    for _ in 0..8 {
        harness.scan().await;
    }
    assert_eq!(harness.owned_services().await, ["other", "web"]);
    assert_eq!(harness.started_jobs().await, [] as [String; 0]);

    // Once the conflict is resolved, the deployment rolls out after all.
    harness.remove_foreign_service(&foreign).await;
    *harness.docker.conflicting_job.lock().await = None;
    harness.finish(&operation).await;
    assert_eq!(
        harness.owned_services().await,
        ["db", "extra", "other", "storage", "web"]
    );
    assert_eq!(harness.started_jobs().await, ["migrate"]);
}

/// A conflict blocking a deployment after its exclusive job succeeded keeps
/// the services the job stopped stopped, as they would run against data in
/// the new format. The deployment continues once the conflict is resolved.
#[tokio::test]
async fn a_conflict_after_an_exclusive_job_succeeded_keeps_its_services_stopped() {
    let harness = ControllerHarness::new().await;
    let first = harness.apply(manifest(), None).await;
    harness.finish(&first).await;
    let mut replacement = manifest_with_exclusive_job();
    let mut extra = replacement.spec.services[0].clone();
    extra.name = "extra".into();
    extra.mounts.clear();
    replacement.spec.services.push(extra);
    harness.docker.hold_jobs.store(true, Ordering::SeqCst);
    let operation = harness.apply(replacement, None).await;
    // The conflict appears while the job runs.
    let conflict = async {
        harness.wait_for_job_starts(1).await;
        let foreign = harness.add_foreign_service(&operation, "extra").await;
        harness.docker.hold_jobs.store(false, Ordering::SeqCst);
        foreign
    };
    let ((), foreign) = tokio::join!(harness.scan(), conflict);
    for _ in 0..3 {
        assert_eq!(
            harness.scan_result(&operation).await,
            (OperationState::Failed, Some("ownership_conflict".into()))
        );
        assert_eq!(harness.owned_services().await, [] as [String; 0]);
    }

    harness.remove_foreign_service(&foreign).await;
    harness.finish(&operation).await;
    assert_eq!(harness.owned_services().await, ["extra", "web"]);
    assert_eq!(harness.started_jobs().await, ["migrate"]);
}

#[tokio::test]
async fn exclusive_job_runs_while_its_volume_users_are_stopped() {
    let harness = ControllerHarness::new().await;
    let first = harness.apply(manifest(), None).await;
    harness.finish(&first).await;
    let operation = harness.apply(manifest_with_exclusive_job(), None).await;
    harness.finish(&operation).await;
    // The running web wrote the volume, so it stopped before the job and
    // restarted with the rollout.
    assert_eq!(
        *harness.docker.job_starts.lock().await,
        [JobStart {
            job: "migrate".into(),
            services: 0,
            networks: 1,
            volumes: 1
        }]
    );
    assert_eq!(harness.docker.observed.lock().await.services.len(), 1);
}

#[tokio::test]
async fn failed_exclusive_job_restarts_stopped_services_once_its_run_stopped() {
    let harness = ControllerHarness::new().await;
    let first = harness.apply(manifest(), None).await;
    harness.finish(&first).await;
    harness.docker.job_exit.store(3, Ordering::SeqCst);
    harness
        .docker
        .fail_job_cleanup
        .store(true, Ordering::SeqCst);
    let operation = harness.apply(manifest_with_exclusive_job(), None).await;
    // While the run may still hold the volume, web stays stopped.
    for _ in 0..3 {
        assert_eq!(
            harness.scan_result(&operation).await,
            (OperationState::Failed, Some("job_failed".into()))
        );
        assert_eq!(
            harness.docker.observed.lock().await.services,
            [] as [ObservedService; 0]
        );
    }
    harness
        .docker
        .fail_job_cleanup
        .store(false, Ordering::SeqCst);
    harness.scan_result(&operation).await;
    assert!(harness.docker.jobs.lock().await.is_empty());
    let observed = harness.docker.observed.lock().await;
    assert_eq!(observed.services.len(), 1);
    assert_eq!(observed.services[0].labels[SERVICE_LABEL], "web");
}

#[tokio::test]
async fn resumed_job_keeps_the_deadline_of_its_start() {
    let mut harness = ControllerHarness::new().await;
    harness.docker.hold_jobs.store(true, Ordering::SeqCst);
    let operation = harness
        .apply(manifest_with_jobs(&["migrate"], 300), None)
        .await;
    let shutdown = CancellationToken::new();
    let (scanned, ()) = tokio::join!(harness.controller.scan(&shutdown), async {
        harness.wait_for_job_starts(1).await;
        shutdown.cancel();
    });
    scanned.unwrap();
    // The daemon stays down past the run's deadline.
    for run in harness.docker.jobs.lock().await.values_mut() {
        run.deadline = std::time::SystemTime::now() - Duration::from_secs(1);
    }
    harness.reopen().await;
    let result = tokio::time::timeout(Duration::from_secs(30), harness.scan_result(&operation))
        .await
        .expect("the resumed run times out at its original deadline");
    assert_eq!(result, (OperationState::Failed, Some("job_timeout".into())));
    assert_eq!(harness.started_jobs().await, ["migrate"]);
}

#[tokio::test]
async fn before_rollout_jobs_gate_service_changes_and_record_output() {
    let harness = ControllerHarness::new().await;
    let mut input = manifest_with_jobs(&["migrate"], 300);
    let first = harness.apply(input.clone(), None).await;
    // A run left by an earlier operation is removed before jobs start.
    harness.docker.jobs.lock().await.insert(
        "leftover".into(),
        JobRunState {
            operation: "earlier-operation".into(),
            exit_code: 0,
            deadline: std::time::SystemTime::now(),
            labels: piqueld_core::resource::Ownership::scope(
                harness.store.instance_id(),
                &first.environment_id,
            ),
        },
    );
    harness.finish(&first).await;
    // The first job sees its network and volume but no service yet.
    assert_eq!(
        *harness.docker.job_starts.lock().await,
        [JobStart {
            job: "migrate".into(),
            services: 0,
            networks: 1,
            volumes: 1,
        }]
    );
    assert!(harness.docker.jobs.lock().await.is_empty());
    let (run, output) = harness.latest_run(&first.environment_id).await;
    assert_eq!(
        (run.job.as_deref(), run.exit_code, run.state),
        (
            Some("migrate"),
            Some(0),
            piqueld_core::api::BuildState::Succeeded
        )
    );
    assert_eq!(output, "migrated\n");

    assert!(!run.log_truncated);

    // A failing job fails the deployment before the running service changes.
    harness.docker.job_exit.store(3, Ordering::SeqCst);
    harness
        .docker
        .truncate_job_output
        .store(true, Ordering::SeqCst);
    input.spec.services[0].replicas = 2.into();
    let failed = harness.apply(input, None).await;
    assert_eq!(
        harness.scan_result(&failed).await,
        (OperationState::Failed, Some("job_failed".into()))
    );
    assert_eq!(harness.docker.observed.lock().await.services[0].replicas, 1);
    assert_eq!(
        harness
            .store
            .get(&first.environment_id)
            .await
            .unwrap()
            .environment
            .resolved_generation,
        Some(1)
    );
    let (run, output) = harness.latest_run(&first.environment_id).await;
    assert_eq!(
        (run.exit_code, run.state),
        (Some(3), piqueld_core::api::BuildState::Failed)
    );
    // Output dropped by Docker is flagged, after Docker's explanation is kept.
    assert!(run.log_truncated);
    assert_eq!(output, "migrated\npiqueld: task: non-zero exit (3)\n");
    assert!(harness.docker.jobs.lock().await.is_empty());
}

#[tokio::test]
async fn a_secret_only_a_job_mounts_is_generated_for_the_job_alone() {
    use piqueld_core::manifest::{SecretDeclaration, SecretEncoding, SecretGenerator, SecretMount};
    let harness = ControllerHarness::new().await;
    let mut input = manifest_with_jobs(&["migrate"], 300);
    input.spec.secrets.push(SecretDeclaration {
        name: "owner-password".into(),
        generate: SecretGenerator::Random {
            bytes: 16,
            encoding: SecretEncoding::Hex,
        },
    });
    input.spec.jobs[0].secrets.push(SecretMount {
        name: "owner-password".into(),
        target: "/run/secrets/owner-password".into(),
    });
    let deployment = harness.apply(input, None).await;
    harness.finish(&deployment).await;
    let target = harness.target(&deployment.environment_id).await;
    let pinned = &target.secret_names["owner-password"];
    assert!(harness.docker.secrets.lock().await.contains_key(pinned));
    assert_eq!(target.services[0].secrets, []);
    assert_eq!(
        target.jobs[0].container.secrets,
        [piqueld_core::resource::SecretFile {
            secret_name: pinned.clone(),
            target: "/run/secrets/owner-password".into(),
        }]
    );
}

#[tokio::test]
async fn failed_job_keeps_an_unchanged_application_degraded() {
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let first = harness
        .apply(manifest_with_jobs(&["migrate"], 300), None)
        .await;
    harness.finish(&first).await;
    harness.docker.job_exit.store(3, Ordering::SeqCst);
    let redeploy = applications
        .deploy(&first.environment_id, None)
        .await
        .unwrap();
    assert_eq!(
        harness.scan_result(&redeploy).await,
        (OperationState::Failed, Some("job_failed".into()))
    );
    // Services already match the target, but the deployment still failed.
    harness.scan_result(&redeploy).await;
    let status = harness.store.status(&first.environment_id).await.unwrap();
    assert_eq!(status.state, piqueld::store::ApplicationState::Degraded);
    assert_eq!(harness.started_jobs().await, ["migrate", "migrate"]);
}

#[tokio::test]
async fn retried_operation_skips_jobs_that_already_succeeded() {
    let harness = ControllerHarness::new().await;
    *harness.docker.conflicting_job.lock().await = Some("seed".into());
    let operation = harness
        .apply(manifest_with_jobs(&["migrate", "seed"], 300), None)
        .await;
    assert_eq!(
        harness.scan_result(&operation).await,
        (OperationState::Failed, Some("ownership_conflict".into()))
    );
    // Planning cannot see the conflict, so the next scan retries the same
    // operation, which runs only the job that has not succeeded yet.
    *harness.docker.conflicting_job.lock().await = None;
    harness.finish(&operation).await;
    assert_eq!(harness.started_jobs().await, ["migrate", "seed"]);
}

/// Waiting for removed job runs to stop leaves the Docker mutation lock
/// free, so other environments' changes and secret deletion carry on.
#[tokio::test]
async fn waiting_for_job_runs_to_stop_leaves_docker_mutations_free() {
    let harness = ControllerHarness::new().await;
    let operation = harness
        .apply(manifest_with_jobs(&["migrate"], 300), None)
        .await;
    let held = harness.docker.hold_job_stop.lock().await;
    let shutdown = CancellationToken::new();
    let scan = harness.controller.scan(&shutdown);
    tokio::pin!(scan);
    tokio::select! {
        result = &mut scan => panic!("scan finished while job runs were stopping: {result:?}"),
        waiting = eventually(Duration::from_secs(10), POLL, async || {
            (harness.docker.job_stop_waits.load(Ordering::SeqCst) > 0).then_some(())
        }) => waiting.expect("the deployment waits for earlier job runs to stop"),
    }
    let runtime = harness.controller.runtime(Arc::default());
    drop(
        tokio::time::timeout(Duration::from_secs(5), runtime.lock_mutations())
            .await
            .expect("the mutation lock is free while job runs stop"),
    );
    drop(held);
    scan.await.unwrap();
    assert_eq!(
        harness.operation(&operation.id).await.state,
        OperationState::Succeeded
    );
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

/// Shutdown lets a deployment waiting for its job record that the run was
/// interrupted, instead of dropping it mid-way.
#[tokio::test]
async fn shutdown_lets_a_running_job_record_its_interruption() {
    let harness = ControllerHarness::new().await;
    harness.docker.hold_jobs.store(true, Ordering::SeqCst);
    let operation = harness
        .apply(manifest_with_jobs(&["migrate"], 300), None)
        .await;
    let shutdown = CancellationToken::new();
    let run = harness.controller.run(
        Arc::default(),
        Duration::from_mins(1),
        0,
        0,
        shutdown.clone(),
    );
    let stop = async {
        harness.wait_for_job_starts(1).await;
        shutdown.cancel();
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(20), async { tokio::join!(run, stop) })
            .await
            .expect("shutdown finishes within its grace period");
    result.unwrap();
    let builds = harness
        .store
        .builds(
            None,
            Some(&operation.environment_id),
            &piqueld_core::access::Scope::All,
            None,
            10,
        )
        .await
        .unwrap();
    assert_eq!(
        builds.items[0].state,
        piqueld_core::api::BuildState::Interrupted
    );
    // The run is kept for the retried deployment to resume.
    assert_eq!(harness.docker.jobs.lock().await.len(), 1);
}

/// A failed migration fails its deployment as soon as its run finished,
/// before its output is read and its run cleaned up. Dropping the deployment
/// there, as a shutdown past its grace or a crash does, keeps the failure,
/// and a restart doesn't run the migration again.
#[tokio::test]
async fn a_failed_job_keeps_its_failure_when_its_deployment_is_dropped() {
    let mut harness = ControllerHarness::new().await;
    harness.docker.job_exit.store(3, Ordering::SeqCst);
    let operation = harness
        .apply(manifest_with_jobs(&["migrate"], 300), None)
        .await;
    let held = harness.docker.hold_job_output.lock().await;
    let shutdown = CancellationToken::new();
    tokio::select! {
        result = harness.controller.scan(&shutdown) => {
            panic!("scan finished while the output read was held: {result:?}");
        }
        reading = eventually(Duration::from_secs(10), POLL, async || {
            (harness.docker.job_output_reads.load(Ordering::SeqCst) > 0).then_some(())
        }) => reading.expect("the failed run's output is read"),
    }
    drop(held);
    harness.reopen().await;
    harness.scan().await;
    let failed = harness.operation(&operation.id).await;
    assert_eq!(failed.state, OperationState::Failed);
    assert_eq!(failed.error_code.as_deref(), Some("job_failed"));
    assert_eq!(harness.started_jobs().await, ["migrate"]);
}

#[tokio::test]
async fn job_run_accepted_before_a_lost_response_is_not_started_again() {
    let harness = ControllerHarness::new().await;
    harness.docker.lose_job_start.store(true, Ordering::SeqCst);
    let operation = harness
        .apply(manifest_with_jobs(&["migrate"], 300), None)
        .await;
    harness.finish(&operation).await;
    assert_eq!(harness.started_jobs().await, ["migrate"]);
}

#[tokio::test]
async fn restarted_daemon_resumes_a_running_job() {
    let mut harness = ControllerHarness::new().await;
    harness.docker.hold_jobs.store(true, Ordering::SeqCst);
    let operation = harness
        .apply(manifest_with_jobs(&["migrate"], 300), None)
        .await;
    let shutdown = CancellationToken::new();
    let (scanned, ()) = tokio::join!(harness.controller.scan(&shutdown), async {
        harness.wait_for_job_starts(1).await;
        shutdown.cancel();
    });
    scanned.unwrap();
    // Shutdown leaves the run in Docker for the restarted daemon.
    assert_eq!(harness.docker.jobs.lock().await.len(), 1);

    harness.reopen().await;
    harness.docker.hold_jobs.store(false, Ordering::SeqCst);
    harness.finish(&operation).await;
    assert_eq!(harness.started_jobs().await, ["migrate"]);
    assert!(harness.docker.jobs.lock().await.is_empty());
    let (run, output) = harness.latest_run(&operation.environment_id).await;
    assert_eq!(run.state, piqueld_core::api::BuildState::Succeeded);
    assert_eq!(output, "migrated\n");
}

#[tokio::test]
async fn superseded_job_run_is_stopped() {
    let harness = ControllerHarness::new().await;
    harness.docker.hold_jobs.store(true, Ordering::SeqCst);
    let first = harness
        .apply(manifest_with_jobs(&["migrate"], 300), None)
        .await;
    let cancellation = CancellationToken::new();
    let (scanned, second) = tokio::join!(harness.controller.scan(&cancellation), async {
        harness.wait_for_job_starts(1).await;
        harness.apply(manifest(), None).await
    });
    scanned.unwrap();
    // The replacement deployment declares no jobs, so the superseded
    // operation must stop its own run.
    assert!(harness.docker.jobs.lock().await.is_empty());
    let (run, _) = harness.latest_run(&first.environment_id).await;
    assert_eq!(run.state, piqueld_core::api::BuildState::Interrupted);
    harness.finish(&second).await;
    assert_eq!(harness.started_jobs().await, ["migrate"]);
}

#[tokio::test]
async fn job_timeout_keeps_output_and_stops_the_run() {
    let harness = ControllerHarness::new().await;
    harness.docker.hold_jobs.store(true, Ordering::SeqCst);
    let operation = harness
        .apply(manifest_with_jobs(&["migrate"], 1), None)
        .await;
    assert_eq!(
        harness.scan_result(&operation).await,
        (OperationState::Failed, Some("job_timeout".into()))
    );
    let (run, output) = harness.latest_run(&operation.environment_id).await;
    assert_eq!(run.state, piqueld_core::api::BuildState::Failed);
    assert_eq!(output, "migrated\n");
    assert!(harness.docker.jobs.lock().await.is_empty());
    assert_eq!(
        harness.docker.observed.lock().await.services,
        [] as [ObservedService; 0]
    );
}

#[tokio::test]
async fn timed_out_job_cleanup_retries_after_restart_without_rerunning() {
    let mut harness = ControllerHarness::new().await;
    harness.docker.hold_jobs.store(true, Ordering::SeqCst);
    harness
        .docker
        .fail_job_cleanup
        .store(true, Ordering::SeqCst);
    let operation = harness
        .apply(manifest_with_jobs(&["migrate"], 1), None)
        .await;
    assert_eq!(
        harness.scan_result(&operation).await,
        (OperationState::Failed, Some("job_timeout".into()))
    );
    let failed = harness.operation(&operation.id).await;
    // Repeated scans retry stopping, without changing the job's failure or
    // starting another run while Docker is unavailable.
    harness.scan().await;
    assert_eq!(harness.docker.jobs.lock().await.len(), 1);
    assert_eq!(harness.operation(&operation.id).await, failed);

    harness.reopen().await;
    harness
        .docker
        .fail_job_cleanup
        .store(false, Ordering::SeqCst);
    harness.scan().await;
    assert!(harness.docker.jobs.lock().await.is_empty());
    assert_eq!(harness.started_jobs().await, ["migrate"]);
    assert_eq!(harness.operation(&operation.id).await, failed);
    assert!(
        harness
            .store
            .get(&operation.environment_id)
            .await
            .unwrap()
            .resolved
            .is_none()
    );
    let (run, output) = harness.latest_run(&operation.environment_id).await;
    assert_eq!(run.state, piqueld_core::api::BuildState::Failed);
    assert_eq!(output, "migrated\n");
}

#[tokio::test]
async fn job_cleanup_still_runs_when_outcome_recording_fails() {
    for trigger in [
        "CREATE TRIGGER reject_job_outcome BEFORE UPDATE OF state ON builds WHEN NEW.job IS NOT NULL AND NEW.state='failed' BEGIN SELECT RAISE(FAIL,'build history unavailable'); END",
        "CREATE TRIGGER reject_job_outcome BEFORE INSERT ON events WHEN NEW.phase='run_job' AND NEW.kind='action_failed' BEGIN SELECT RAISE(FAIL,'journal unavailable'); END",
    ] {
        let harness = ControllerHarness::new().await;
        let mut connection = harness.database().await;
        sqlx::query(trigger).execute(&mut connection).await.unwrap();
        harness.docker.hold_jobs.store(true, Ordering::SeqCst);
        let operation = harness
            .apply(manifest_with_jobs(&["migrate"], 1), None)
            .await;
        assert_eq!(
            harness.scan_result(&operation).await,
            (OperationState::Failed, Some("job_timeout".into()))
        );
        assert!(harness.docker.jobs.lock().await.is_empty());
        sqlx::query("DROP TRIGGER reject_job_outcome")
            .execute(&mut connection)
            .await
            .unwrap();
        harness.scan().await;
        assert_eq!(harness.started_jobs().await, ["migrate"]);
    }
}

#[tokio::test]
async fn pending_cleanup_blocks_the_next_job() {
    let harness = ControllerHarness::new().await;
    harness
        .docker
        .fail_job_cleanup
        .store(true, Ordering::SeqCst);
    let operation = harness
        .apply(manifest_with_jobs(&["migrate", "seed"], 300), None)
        .await;
    assert_eq!(
        harness.scan_result(&operation).await,
        (OperationState::Failed, Some("docker_request_failed".into()))
    );
    assert_eq!(harness.started_jobs().await, ["migrate"]);
    assert_eq!(harness.docker.jobs.lock().await.len(), 1);
    // A still-failing cleanup on a due deployment retry must refresh the
    // deployment's backoff, while keeping the successful job skipped.
    harness.expire_backoff(&operation).await;
    assert_eq!(
        harness.scan_result(&operation).await,
        (OperationState::Failed, Some("docker_request_failed".into()))
    );
    assert_eq!(
        harness.operation(&operation.id).await.consecutive_failures,
        2
    );
    assert_eq!(harness.started_jobs().await, ["migrate"]);
    harness
        .docker
        .fail_job_cleanup
        .store(false, Ordering::SeqCst);
    harness.expire_backoff(&operation).await;
    harness.finish(&operation).await;
    assert_eq!(harness.started_jobs().await, ["migrate", "seed"]);
    assert!(harness.docker.jobs.lock().await.is_empty());
}

#[tokio::test]
async fn unknown_job_status_keeps_the_run_for_a_retry() {
    let harness = ControllerHarness::new().await;
    harness.docker.hold_jobs.store(true, Ordering::SeqCst);
    harness.docker.fail_job_status.store(true, Ordering::SeqCst);
    let operation = harness
        .apply(manifest_with_jobs(&["migrate"], 1), None)
        .await;
    // Docker never reported the status, so the outcome is unknown: the
    // failure is transient and the run keeps going.
    assert_eq!(
        harness.scan_result(&operation).await,
        (OperationState::Failed, Some("docker_request_failed".into()))
    );
    let (run, _) = harness.latest_run(&operation.environment_id).await;
    assert_eq!(run.state, piqueld_core::api::BuildState::Interrupted);
    assert_eq!(harness.docker.jobs.lock().await.len(), 1);

    // The retry resumes the run instead of starting it again.
    harness
        .docker
        .fail_job_status
        .store(false, Ordering::SeqCst);
    harness.docker.hold_jobs.store(false, Ordering::SeqCst);
    harness.expire_backoff(&operation).await;
    harness.finish(&operation).await;
    assert_eq!(harness.started_jobs().await, ["migrate"]);
    let (run, _) = harness.latest_run(&operation.environment_id).await;
    assert_eq!(run.state, piqueld_core::api::BuildState::Succeeded);
    assert!(harness.docker.jobs.lock().await.is_empty());
}

#[tokio::test]
async fn recorded_success_survives_a_crash_before_cleanup() {
    let mut harness = ControllerHarness::new().await;
    let operation = harness
        .apply(manifest_with_jobs(&["migrate"], 300), None)
        .await;
    let held = harness.docker.hold_job_cleanup.lock().await;
    // The success is stored before the run is removed; dropping the scan
    // there is a crash.
    let succeeded = eventually(Duration::from_secs(10), POLL, async || {
        let builds = harness
            .store
            .builds(
                None,
                Some(&operation.environment_id),
                &piqueld_core::access::Scope::All,
                None,
                10,
            )
            .await
            .unwrap();
        builds
            .items
            .first()
            .is_some_and(|run| run.state == piqueld_core::api::BuildState::Succeeded)
            .then_some(())
    });
    let shutdown = CancellationToken::new();
    tokio::select! {
        result = harness.controller.scan(&shutdown) => {
            panic!("scan finished while cleanup was held: {result:?}");
        }
        recorded = succeeded => {
            recorded.expect("success recorded while cleanup was held");
        }
    }
    drop(held);
    assert_eq!(harness.docker.jobs.lock().await.len(), 1);

    harness.reopen().await;
    harness.finish(&operation).await;
    assert_eq!(harness.started_jobs().await, ["migrate"]);
    assert!(harness.docker.jobs.lock().await.is_empty());
}

#[tokio::test]
async fn retry_reruns_a_failed_run_left_in_place() {
    let harness = ControllerHarness::new().await;
    harness.docker.job_exit.store(3, Ordering::SeqCst);
    harness
        .docker
        .fail_job_cleanup
        .store(true, Ordering::SeqCst);
    let operation = harness
        .apply(manifest_with_jobs(&["migrate"], 300), None)
        .await;
    assert_eq!(
        harness.scan_result(&operation).await,
        (OperationState::Failed, Some("job_failed".into()))
    );
    assert_eq!(harness.docker.jobs.lock().await.len(), 1);
    // The failed run's outcome is recorded, so retrying the operation starts
    // a new run.
    harness.docker.job_exit.store(0, Ordering::SeqCst);
    harness
        .docker
        .fail_job_cleanup
        .store(false, Ordering::SeqCst);
    let retry = harness
        .applications()
        .reconcile(&operation.environment_id, None)
        .await
        .unwrap();
    assert_eq!(retry.id, operation.id);
    harness.finish(&operation).await;
    assert_eq!(harness.started_jobs().await, ["migrate", "migrate"]);
}

#[tokio::test]
async fn job_outcome_survives_output_and_cleanup_failures() {
    let harness = ControllerHarness::new().await;
    harness.docker.fail_job_output.store(true, Ordering::SeqCst);
    harness
        .docker
        .fail_job_cleanup
        .store(true, Ordering::SeqCst);
    let operation = harness
        .apply(manifest_with_jobs(&["migrate"], 300), None)
        .await;
    harness.finish(&operation).await;
    let (run, output) = harness.latest_run(&operation.environment_id).await;
    assert_eq!(
        (run.exit_code, run.state),
        (Some(0), piqueld_core::api::BuildState::Succeeded)
    );
    assert!(
        output.starts_with("piqueld: job output unavailable"),
        "{output}"
    );
    assert_eq!(harness.docker.jobs.lock().await.len(), 1);
    assert_eq!(harness.started_jobs().await, ["migrate"]);
    // A periodic scan removes the leftover without another deployment.
    harness
        .docker
        .fail_job_cleanup
        .store(false, Ordering::SeqCst);
    harness.finish(&operation).await;
    assert_eq!(harness.started_jobs().await, ["migrate"]);
    assert!(harness.docker.jobs.lock().await.is_empty());
}

#[tokio::test]
async fn blocked_deployment_runs_no_jobs() {
    let harness = ControllerHarness::new().await;
    let operation = harness
        .apply(manifest_with_jobs(&["migrate"], 300), None)
        .await;
    harness
        .docker
        .observed
        .lock()
        .await
        .networks
        .push(ObservedNetwork {
            name: piqueld_core::DockerNetworkName::for_application(&operation.environment_id)
                .to_string(),
            runtime_configuration_matches: true,
            labels: foreign_labels(&operation.environment_id),
            in_use: false,
        });
    assert_eq!(
        harness.scan_result(&operation).await,
        (OperationState::Failed, Some("ownership_conflict".into()))
    );
    assert_eq!(harness.started_jobs().await, [] as [String; 0]);
}

/// Image retention: cleanup removes only this installation's unretained
/// builds, never while a preparation may record one, and deployments that
/// use a retained image again find it or fail naming it.
mod image_retention {
    use super::*;
    use piqueld::reconcile::ImagesInUse;
    use piqueld_core::{ServiceName, api::ImageStatus};

    /// A local image with ID `byte`s and `labels`.
    fn image(byte: char, labels: &[(&str, &str)]) -> LocalImage {
        LocalImage {
            id: Sha256Digest::parse(format!("sha256:{}", byte.to_string().repeat(64))).unwrap(),
            repo_digests: Vec::new(),
            labels: labels
                .iter()
                .map(|(key, value)| ((*key).into(), (*value).into()))
                .collect(),
            size: 10,
            used: false,
        }
    }

    impl ControllerHarness {
        /// Deploys the fixture with `web` built from Git, as image `b`s.
        async fn deploy_git_build(&self, fixture: &git_fixture::GitBuildFixture) -> Operation {
            let mut input = manifest();
            input.spec.services[0].source = fixture.source.clone();
            self.apply(input, Some(0)).await
        }

        /// IDs of the images left in the engine.
        async fn image_ids(&self) -> Vec<char> {
            self.docker
                .local_images
                .lock()
                .await
                .iter()
                .map(|image| image.id.as_str().chars().last().unwrap())
                .collect()
        }
    }

    #[tokio::test]
    async fn cleanup_removes_only_this_installations_unretained_builds() {
        let harness = ControllerHarness::new().await;
        let fixture = git_fixture::GitBuildFixture::new();
        let operation = harness.deploy_git_build(&fixture).await;
        harness.finish(&operation).await;
        let ours = harness.store.instance_id().to_owned();
        let owned = [(MANAGED_LABEL, "true"), (INSTANCE_LABEL, ours.as_str())];
        let used = LocalImage {
            used: true,
            ..image('f', &owned)
        };
        harness.docker.local_images.lock().await.extend([
            image('c', &owned),
            // Another installation or development instance on the same engine.
            image(
                'd',
                &[(MANAGED_LABEL, "true"), (INSTANCE_LABEL, "instance-other")],
            ),
            // Built by hand, or by piqueld before builds were labelled.
            image('e', &[]),
            used,
        ]);

        assert!(harness.controller.clean_images().await.unwrap());
        // `b` runs in production; `f` is a stopped container's.
        assert_eq!(harness.image_ids().await, ['b', 'd', 'e', 'f']);
        let status = harness.controller.image_status().borrow().clone();
        assert_eq!((status.images, status.reclaimed_bytes), (2, 10));
        let events = harness.store.events(None, None, 100).await.unwrap().items;
        let removed = events
            .iter()
            .filter(|event| {
                event.phase.as_deref() == Some("remove_image") && event.kind == "action_succeeded"
            })
            .filter_map(|event| event.resource.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(removed, [image('c', &[]).id.as_str()]);
    }

    #[tokio::test]
    async fn an_image_built_for_a_deployment_in_preparation_survives_cleanup() {
        let gate = Arc::new(Gate::default());
        let harness = ControllerHarness::with_docker(FakeDocker {
            build_gate: Some(Arc::clone(&gate)),
            ..FakeDocker::default()
        })
        .await;
        let fixture = git_fixture::GitBuildFixture::new();
        let operation = harness.deploy_git_build(&fixture).await;
        let cancellation = CancellationToken::new();
        let scan = harness.controller.scan(&cancellation);
        tokio::pin!(scan);
        tokio::select! {
            () = gate.entered.notified() => {}
            result = &mut scan => panic!("scan finished before building: {result:?}"),
        }
        // Built, but no retention root records it yet: cleanup waits.
        assert_eq!(harness.image_ids().await, ['b']);
        assert!(!harness.controller.clean_images().await.unwrap());
        assert_eq!(harness.image_ids().await, ['b']);
        assert_eq!(
            harness.controller.image_status().borrow().clone(),
            ImageStatus::default()
        );

        gate.release.notify_one();
        scan.await.unwrap();
        assert_eq!(
            harness.operation(&operation.id).await.state,
            OperationState::Succeeded
        );
        assert!(harness.controller.clean_images().await.unwrap());
        assert_eq!(harness.image_ids().await, ['b']);
    }

    #[tokio::test]
    async fn missing_images_are_pulled_again_by_digest_or_reported_unavailable() {
        let harness = ControllerHarness::new().await;
        let in_use: ImagesInUse = harness.controller.images_in_use().await;
        let web = ServiceName::parse("web").unwrap();
        let pulled = prebuilt::resolutions(&harness.application).sources[&web].clone();
        let digest = pulled.repository_digest().unwrap().to_string();

        in_use
            .ensure(&*harness.docker, [(&web, &pulled)])
            .await
            .unwrap();
        assert_eq!(harness.docker.registry.lock().await.pulls[&digest], 1);

        let api = ServiceName::parse("api").unwrap();
        let built = ResolvedSource::Git {
            requested: piqueld_core::manifest::ValidatedSource::Image {
                image: "unused".into(),
            },
            commit: "a".repeat(40),
            image_id: image('c', &[]).id,
        };
        let error = in_use
            .ensure(&*harness.docker, [(&web, &pulled), (&api, &built)])
            .await
            .unwrap_err();
        assert_eq!(error.code(), "image_unavailable");
        assert_eq!(
            error.message(),
            format!(
                "service api's image sha256:{} is no longer present, and a build can't be pulled again",
                "c".repeat(64)
            )
        );
        // `web` was present this time.
        assert_eq!(harness.docker.registry.lock().await.pulls[&digest], 1);
    }
}

/// Promoting releases into promoted environments, which never build or fetch.
mod promotion {
    use super::*;
    use piqueld::api::ApplicationError;
    use piqueld::store::{PromotionError, StoreError};
    use piqueld_core::api::{
        AcceptedPromotion, DeploymentOrigin, DeploymentView, EnvironmentAccess, PromotionSelection,
        SecretAccess, SecretProblem,
    };
    use piqueld_core::{ReleaseId, manifest::parse_template_toml};

    /// `shop`, whose `web` service renders `ORIGIN` from each environment's
    /// `domain`, with more `staging` and `production` variables, then `spec`.
    /// It declares `staging`, and `production` promoted from it.
    fn shop_toml(staging: &str, production: &str, spec: &str) -> String {
        format!(
            "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='shop'\n\
             [spec.environments.staging.variables]\ndomain='staging.example.com'\n{staging}\n\
             [spec.environments.production]\n{PROMOTED}\
             [spec.environments.production.variables]\ndomain='example.com'\n{production}\n\
             [[spec.services]]\nname='web'\n[spec.services.environment]\nORIGIN='https://${{{{ vars.domain }}}}'\n{spec}"
        )
    }

    /// Declares `production` promoted from `staging` in [`shop_toml`].
    const PROMOTED: &str = "promote_from='staging'\n";

    /// The image source every `shop` deploys unless a test changes it.
    const IMAGE: &str = "[spec.services.source]\ntype='image'\nimage='ghcr.io/example/notes:1.4.0'";

    /// `shop` saved from `manifest`, which declares `staging` and
    /// `production`.
    struct Shop {
        harness: ControllerHarness,
        application: ApplicationId,
        staging: EnvironmentId,
        production: EnvironmentId,
    }

    impl Shop {
        async fn new(manifest: &str) -> Self {
            let harness = ControllerHarness::new().await;
            // Boxed: saving holds a whole manifest across awaits.
            let saved = Box::pin(
                harness
                    .applications()
                    .save(parse_template_toml(manifest).unwrap(), Some(0)),
            )
            .await
            .unwrap();
            let application = ApplicationId::parse(saved.application_id).unwrap();
            let environments = harness.store.environments(&application).await.unwrap();
            let named = |name: &str| {
                environments
                    .iter()
                    .find(|environment| environment.name.as_str() == name)
                    .unwrap()
                    .id
                    .clone()
            };
            let (staging, production) = (named("staging"), named("production"));
            Self {
                harness,
                application,
                staging,
                production,
            }
        }

        /// Saves `manifest` again, converging the environments it declares.
        async fn save(&self, manifest: &str) {
            self.harness
                .applications()
                .save(parse_template_toml(manifest).unwrap(), None)
                .await
                .unwrap();
        }

        /// Deploys `staging` until it succeeds.
        async fn deploy_staging(&self) -> Operation {
            let operation = self
                .harness
                .applications()
                .deploy(&self.staging, None)
                .await
                .unwrap();
            self.harness.finish(&operation).await;
            operation
        }

        /// Promotes `selection` into `production`.
        async fn promote(
            &self,
            selection: PromotionSelection,
        ) -> Result<AcceptedPromotion, ApplicationError> {
            self.harness
                .applications()
                .promote(Daemon, &self.production, selection, None, true, None)
                .await
        }

        /// Promotes the source's current release.
        async fn promote_current(&self) -> Result<AcceptedPromotion, ApplicationError> {
            self.promote(PromotionSelection::Source { deployment: None })
                .await
        }

        /// Promotes the source's current release and runs it until it succeeds.
        async fn promoted(&self) -> AcceptedPromotion {
            let accepted = self.promote_current().await.unwrap();
            let operation = self
                .harness
                .operation(&accepted.operation.operation_id)
                .await;
            self.harness.finish(&operation).await;
            accepted
        }

        async fn deployment(&self, id: &str) -> DeploymentView {
            let environment = self
                .harness
                .store
                .operation(id)
                .await
                .unwrap()
                .environment_id;
            self.harness
                .store
                .deployments(&environment, None, 50)
                .await
                .unwrap()
                .items
                .into_iter()
                .find(|deployment| deployment.operation.id == id)
                .unwrap()
        }

        /// The value `environment`'s running `web` gets for `ORIGIN`, and its image.
        async fn web(&self, environment: &EnvironmentId) -> (String, String) {
            let target = self.harness.target(environment).await;
            let web = &target.services[0];
            (web.environment["ORIGIN"].clone(), web.image.to_string())
        }

        async fn release(&self, deployment: &str) -> ReleaseId {
            self.deployment(deployment).await.release.unwrap()
        }
    }

    /// The promotion error a request failed with.
    fn refusal(result: Result<AcceptedPromotion, ApplicationError>) -> PromotionError {
        match result {
            Err(ApplicationError::Store(StoreError::Promotion(error))) => error,
            other => panic!("promotion refusal expected, got {other:?}"),
        }
    }

    /// The release staging built is deployed to production as recorded:
    /// rendered with production's block, its image never pulled or built
    /// again, except a missing registry image, pulled again by digest at
    /// acceptance once the request's revision holds. Each deployment records where it came from, and the
    /// release where it was promoted.
    #[tokio::test]
    async fn promotes_the_source_release_rendered_for_the_target_without_building() {
        let shop = Shop::new(&shop_toml("", "", IMAGE)).await;
        let promote = async |generation, request_id| {
            shop.harness
                .applications()
                .promote(
                    Daemon,
                    &shop.production,
                    PromotionSelection::Source { deployment: None },
                    Some(generation),
                    false,
                    request_id,
                )
                .await
        };
        let stale = async || {
            assert!(matches!(
                promote(0, None).await,
                Err(ApplicationError::Store(
                    StoreError::GenerationConflict { .. }
                ))
            ));
        };
        // A stale revision is refused first, even before the source deployed.
        stale().await;
        let built = shop.deploy_staging().await;
        let release = shop.release(&built.id).await;
        let image = shop.web(&shop.staging).await.1;
        // Before its first promotion, production shows the manifest the
        // release its source runs would deploy.
        let next = shop
            .harness
            .store
            .release(&shop.application, &release)
            .await
            .unwrap()
            .release
            .template()
            .clone();
        let detail = shop
            .harness
            .applications()
            .environment_detail(&shop.production, &piqueld_core::access::Scope::All)
            .await
            .unwrap();
        assert_eq!(detail.manifest, Some(next));
        let pulls = shop.harness.pulls().await;
        // Cleanup or an operator removed the pulled image meanwhile.
        shop.harness.docker.local_images.lock().await.clear();
        // And before the image is pulled again, like a malformed idempotency
        // key.
        stale().await;
        let generation = shop.harness.store.get(&shop.production).await;
        let generation = generation.unwrap().application.generation;
        assert!(matches!(
            promote(generation, Some("not a key")).await,
            Err(ApplicationError::Store(StoreError::InvalidInput))
        ));
        assert_eq!(shop.harness.pulls().await, pulls);

        let accepted = shop.promoted().await;
        assert_eq!(accepted.release_id, release);
        let origin = DeploymentOrigin::Promotion {
            environment: shop.staging.clone(),
            deployment: built.id.clone(),
        };
        assert_eq!(accepted.origin, origin);
        assert_eq!(shop.harness.pulls().await, pulls + 1);
        assert!(!shop.harness.docker.local_images.lock().await.is_empty());

        assert_eq!(
            shop.web(&shop.production).await,
            ("https://example.com".to_owned(), image.clone())
        );
        assert_eq!(
            shop.web(&shop.staging).await,
            ("https://staging.example.com".to_owned(), image)
        );
        let promoted = shop.deployment(&accepted.operation.operation_id).await;
        assert_eq!(promoted.release, Some(release.clone()));
        assert_eq!(promoted.origin, origin);
        assert_eq!(
            shop.deployment(&built.id).await.origin,
            DeploymentOrigin::Build
        );
        let releases = shop
            .harness
            .store
            .releases(&shop.application, None, 20)
            .await
            .unwrap();
        let [listed] = releases.items.as_slice() else {
            panic!("one release expected, got {releases:?}")
        };
        assert_eq!(
            listed
                .promotions
                .iter()
                .map(|promotion| (&promotion.environment_id, &promotion.origin))
                .collect::<Vec<_>>(),
            [(&shop.production, &origin)]
        );
    }

    /// A later source deployment never retargets an accepted promotion, a
    /// reviewed deployment that is no longer current is refused, and an
    /// earlier release can be deployed again by ID.
    #[tokio::test]
    async fn the_release_is_pinned_at_acceptance() {
        let shop = Shop::new(&shop_toml("", "", IMAGE)).await;
        let first = shop.deploy_staging().await;
        let release = shop.release(&first.id).await;
        let applications = shop.harness.applications();
        let promote = |request| {
            applications.promote(
                Daemon,
                &shop.production,
                PromotionSelection::Source {
                    deployment: Some(first.id.clone()),
                },
                None,
                true,
                Some(request),
            )
        };
        let accepted = promote("promote-first").await.unwrap();

        // The tag now resolves to another image, so staging's next deployment
        // records another release. Running it also runs the promotion.
        shop.harness
            .docker
            .registry
            .lock()
            .await
            .digests
            .insert("ghcr.io/example/notes:1.4.0".into(), "b".repeat(64));
        let second = shop.deploy_staging().await;
        assert_ne!(shop.release(&second.id).await, release);
        let promoted = shop
            .harness
            .operation(&accepted.operation.operation_id)
            .await;
        assert_eq!(promoted.state, OperationState::Succeeded);
        assert_eq!(
            shop.deployment(&promoted.id).await.release,
            Some(release.clone())
        );
        assert!(
            shop.web(&shop.production)
                .await
                .1
                .ends_with(&"a".repeat(64))
        );

        assert!(matches!(
            refusal(
                shop.promote(PromotionSelection::Source {
                    deployment: Some(first.id.clone()),
                })
                .await
            ),
            PromotionError::SourceChanged { deployment, .. } if deployment == first.id
        ));
        // Repeating the accepted request returns its response all the same.
        let repeated = promote("promote-first").await.unwrap();
        assert_eq!(
            repeated.operation.operation_id,
            accepted.operation.operation_id
        );

        let again = shop
            .promote(PromotionSelection::Release {
                release: release.clone(),
            })
            .await
            .unwrap();
        let operation = shop.harness.operation(&again.operation.operation_id).await;
        shop.harness.finish(&operation).await;
        assert_eq!(again.origin, DeploymentOrigin::Release);
        assert_eq!(
            shop.deployment(&operation.id).await.origin,
            DeploymentOrigin::Release
        );
        assert!(
            shop.web(&shop.production)
                .await
                .1
                .ends_with(&"a".repeat(64))
        );
    }

    /// Every precondition refuses the promotion before anything is captured:
    /// a source without a succeeded deployment or with unhealthy services, a
    /// build input that renders differently, and missing or inaccessible
    /// secrets, all listed at once (also by the plan).
    #[tokio::test]
    async fn preconditions_are_checked_before_anything_is_captured() {
        let manifest = shop_toml(
            "api='staging-api'",
            "api='production-api'",
            &format!(
                "{IMAGE}\n[[spec.services.secrets]]\nname='${{{{ vars.api }}}}'\ntarget='/run/secrets/api'\n\
                 [[spec.services.secrets]]\nname='${{{{ vars.db }}}}'\ntarget='/run/secrets/db'\n\
                 [spec.variables]\ndb='shared-db'"
            ),
        );
        let shop = Shop::new(&manifest).await;
        assert!(matches!(
            refusal(shop.promote_current().await),
            PromotionError::SourceNotReady { .. }
        ));
        let applications = shop.harness.applications();
        applications
            .put_stored_secret(
                Daemon,
                &shop.application,
                "staging-api",
                0,
                b"api".to_vec(),
                None,
            )
            .await
            .unwrap();
        let staging_only = SecretAccess {
            environments: EnvironmentAccess::Only([shop.staging.clone()].into()),
            previews: false,
        };
        applications
            .put_stored_secret(
                Daemon,
                &shop.application,
                "shared-db",
                0,
                b"db".to_vec(),
                Some(&staging_only),
            )
            .await
            .unwrap();
        shop.deploy_staging().await;

        let expected = [
            SecretProblem::Missing {
                secret: "production-api".into(),
            },
            SecretProblem::AccessDenied {
                secret: "shared-db".into(),
            },
        ];
        match refusal(shop.promote_current().await) {
            PromotionError::Secrets { secrets, .. } => assert_eq!(secrets, expected),
            other => panic!("secrets refusal expected, got {other:?}"),
        }
        let plan = applications
            .plan_release(
                &shop.production,
                &PromotionSelection::Source { deployment: None },
            )
            .await
            .unwrap();
        assert_eq!(plan.release.unwrap().secrets, expected);

        // Unhealthy right now, though its deployment succeeded; then healthy,
        // but running another image than its release's.
        shop.harness.docker.observed.lock().await.services[0].convergence = Convergence::Degraded;
        assert!(matches!(
            refusal(shop.promote_current().await),
            PromotionError::SourceNotReady { .. }
        ));
        {
            let mut observed = shop.harness.docker.observed.lock().await;
            let web = &mut observed.services[0];
            web.convergence = Convergence::Converged;
            web.image = format!("ghcr.io/example/notes@sha256:{}", "f".repeat(64));
        }
        assert!(matches!(
            refusal(shop.promote_current().await),
            PromotionError::SourceNotReady { .. }
        ));
        assert!(
            shop.harness
                .store
                .latest_operation_for_environment(&shop.production)
                .await
                .unwrap()
                .is_none()
        );
    }

    /// The environment's declared secrets are generated for a promotion, but
    /// stored only once it is accepted.
    #[tokio::test]
    async fn generated_secrets_are_stored_with_the_accepted_promotion() {
        let manifest = shop_toml(
            "",
            "",
            &format!(
                "{IMAGE}\n[[spec.services.secrets]]\nname='session'\ntarget='/run/secrets/session'\n\
                 [[spec.secrets]]\nname='session'\n[spec.secrets.generate]\ntype='random'\nbytes=16"
            ),
        );
        let shop = Shop::new(&manifest).await;
        shop.deploy_staging().await;
        let applications = shop.harness.applications();
        let current = PromotionSelection::Source { deployment: None };
        assert!(matches!(
            applications
                .promote(
                    Daemon,
                    &shop.production,
                    current.clone(),
                    Some(0),
                    false,
                    None
                )
                .await,
            Err(ApplicationError::Store(
                StoreError::GenerationConflict { .. }
            ))
        ));
        assert!(
            shop.harness
                .store
                .secrets(&shop.production)
                .await
                .unwrap()
                .is_empty()
        );
        shop.promoted().await;
        let stored = shop.harness.store.secrets(&shop.production).await.unwrap();
        assert_eq!(
            stored
                .iter()
                .map(|secret| secret.name.as_str())
                .collect::<Vec<_>>(),
            ["session"]
        );
        // Planning the same release again changes nothing: services keep the
        // secret version they run.
        let plan = applications
            .plan_release(&shop.production, &current)
            .await
            .unwrap();
        assert!(plan.identical, "{:?}", plan.changes);
        assert!(
            !plan.plan.actions.iter().any(|action| matches!(
                action.kind,
                piqueld_core::ActionKind::EnsureService { .. }
            )),
            "{:?}",
            plan.plan.actions
        );
    }

    /// A build input production renders differently can't reuse the
    /// release's image.
    #[tokio::test]
    async fn a_release_whose_build_inputs_render_differently_is_refused() {
        let tagged = shop_toml(
            "",
            "tag='1.5.0'",
            "[spec.services.source]\ntype='image'\nimage='ghcr.io/example/notes:${{ vars.tag }}'\n\
             [spec.variables]\ntag='1.4.0'",
        );
        let shop = Shop::new(&tagged).await;
        shop.deploy_staging().await;
        match shop.promote_current().await {
            Err(ApplicationError::Store(StoreError::Validation(errors))) => assert_eq!(
                errors
                    .0
                    .iter()
                    .map(|error| (error.code.as_str(), error.path.as_str()))
                    .collect::<Vec<_>>(),
                [("release_incompatible", "web.source.image")]
            ),
            other => panic!("release_incompatible expected, got {other:?}"),
        }
    }

    /// An environment can't become promoted while a deployment may be
    /// fetching or building: its declaration waits for the next convergence.
    /// Retrying a deployment that never prepared fails instead of fetching or
    /// building once it is promoted.
    #[tokio::test]
    async fn a_promoted_environment_never_builds_a_deployment_requested_before() {
        let promoted = shop_toml("", "", IMAGE);
        let tracking = promoted.replace(PROMOTED, "");
        let shop = Shop::new(&tracking).await;
        let requested = shop
            .harness
            .applications()
            .deploy(&shop.production, None)
            .await
            .unwrap();
        let source = || async {
            shop.harness
                .store
                .get(&shop.production)
                .await
                .unwrap()
                .environment
                .source
                .promoted_from()
                .cloned()
        };
        shop.save(&promoted).await;
        assert_eq!(source().await, None);

        // It fails before preparing anything, then the environment is promoted.
        shop.harness
            .docker
            .incompatible_swarm
            .store(true, Ordering::SeqCst);
        shop.harness.scan().await;
        let failed = shop.harness.operation(&requested.id).await;
        assert_eq!(failed.state, OperationState::Failed);
        shop.harness
            .docker
            .incompatible_swarm
            .store(false, Ordering::SeqCst);
        shop.save(&promoted).await;
        assert_eq!(source().await, Some(shop.staging.clone()));

        let pulls = shop.harness.pulls().await;
        let retried = shop
            .harness
            .applications()
            .reconcile(&shop.production, None)
            .await
            .unwrap();
        assert_eq!(retried.id, requested.id);
        shop.harness.scan().await;
        let failed = shop.harness.operation(&requested.id).await;
        assert_eq!(failed.state, OperationState::Failed);
        assert_eq!(failed.error_code.as_deref(), Some("environment_promoted"));
        assert_eq!(shop.harness.pulls().await, pulls);
    }

    /// An environment made promoted still repairs drift in the build it
    /// runs: converging its active target again rolls nothing new in.
    #[tokio::test]
    async fn a_promoted_environment_repairs_the_build_it_runs() {
        let promoted = shop_toml("", "", IMAGE);
        let tracking = promoted.replace(PROMOTED, "");
        let shop = Shop::new(&tracking).await;
        let deployed = shop
            .harness
            .applications()
            .deploy(&shop.production, None)
            .await
            .unwrap();
        let deployed = shop.harness.operation(&deployed.id).await;
        shop.harness.finish(&deployed).await;
        shop.save(&promoted).await;
        let production = shop.production.to_string();
        let web = async || {
            let mut observed = shop.harness.docker.observed.lock().await;
            let web = observed
                .services
                .iter_mut()
                .find(|service| service.labels.get(APPLICATION_LABEL) == Some(&production))
                .unwrap();
            let replicas = web.replicas;
            web.replicas = 0;
            web.convergence = Convergence::Degraded;
            replicas
        };
        web().await;
        shop.harness.finish(&deployed).await;
        assert_eq!(web().await, 1);
    }

    /// A build that prepared its target before the environment was made
    /// promoted doesn't roll out into it when retried: a promoted
    /// environment only runs releases.
    #[tokio::test]
    async fn a_promoted_environment_never_rolls_out_a_build_prepared_before() {
        let promoted = shop_toml("", "", IMAGE);
        let tracking = promoted.replace(PROMOTED, "");
        let mut shop = Shop::new(&tracking).await;
        shop.harness.set_convergence_timeout(Duration::from_secs(1));
        let requested = shop
            .harness
            .applications()
            .deploy(&shop.production, None)
            .await
            .unwrap();
        // It prepares, then fails before rolling out.
        shop.harness
            .docker
            .fail_observations
            .store(true, Ordering::SeqCst);
        shop.harness.scan().await;
        assert_eq!(
            shop.harness.operation(&requested.id).await.state,
            OperationState::Failed
        );
        assert!(
            shop.harness
                .store
                .prepared_target(&requested.id)
                .await
                .unwrap()
                .is_some()
        );
        shop.harness
            .docker
            .fail_observations
            .store(false, Ordering::SeqCst);
        shop.save(&promoted).await;

        let retried = shop
            .harness
            .applications()
            .reconcile(&shop.production, None)
            .await
            .unwrap();
        assert_eq!(retried.id, requested.id);
        shop.harness.scan().await;
        let failed = shop.harness.operation(&requested.id).await;
        assert_eq!(failed.state, OperationState::Failed);
        assert_eq!(failed.error_code.as_deref(), Some("environment_promoted"));
        assert!(
            shop.harness
                .store
                .get(&shop.production)
                .await
                .unwrap()
                .resolved
                .is_none()
        );
    }
}

/// Runs the contract scenario `$scenario` against the fake, whose registry
/// lacks the contract's missing image and serves its image at a digest
/// other than the base one, as unpinned references resolve by default.
macro_rules! contract_test {
    ($scenario:ident) => {
        #[tokio::test]
        async fn $scenario() {
            let docker = crate::FakeDocker::default();
            {
                let mut registry = docker.registry.lock().await;
                registry
                    .missing
                    .insert(crate::docker_contract::MISSING_IMAGE.into());
                registry
                    .digests
                    .insert(crate::docker_contract::IMAGE.into(), "c".repeat(64));
            }
            crate::docker_contract::Contract::new(docker)
                .await
                .$scenario()
                .await;
        }
    };
}

/// The fake behaves like the real Engine: see [`docker_contract`].
mod contract {
    crate::docker_contract::scenarios!(contract_test);
}
