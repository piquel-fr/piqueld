//! Reconciliation coverage using the real Docker seam and an in-memory backend.

#[path = "../support/application.rs"]
mod application_fixture;
#[path = "../support/docker_contract.rs"]
mod docker_contract;
#[path = "../support/git.rs"]
mod git_fixture;
#[path = "../support/poll.rs"]
mod poll;
#[path = "../support/prebuilt.rs"]
mod prebuilt;
#[path = "../support/repository.rs"]
mod repository;
#[path = "../support/sqlite.rs"]
mod sqlite;

use application_fixture::TestApplications;
use async_trait::async_trait;
use piqueld::api::Actor::Daemon;
use piqueld::git::Sources;
use piqueld::reconcile::Controller;
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
use piqueld_docker::fake::{
    FakeDocker, Gate, JobRunState, JobStart, RegistryState, observed_service,
};
use piqueld_docker::{DockerApi, DockerError, ImageSource, LocalImage, resolve_image_digest};
use piqueld_store::Store;
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
            stop_service: false,
        })
        .collect();
    input
}

/// The fixture manifest with one `migrate` job of `web`, run before rollout.
fn migration() -> ApplicationManifest {
    manifest_with_jobs(&["migrate"], 300)
}

/// A store, the prebuilt fixture, and a controller over a fake Docker
/// backend.
struct ControllerHarness {
    _directory: tempfile::TempDir,
    database_path: PathBuf,
    store: Arc<Store>,
    sources: Arc<Sources>,
    application: NormalizedApplication,
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
            Store::open_unsynced(&database_path)
                .await
                .expect("fresh database opens"),
        );
        let sources = Arc::new(Sources::open(directory.path()).await.unwrap());
        let application = prebuilt::application("app-fake-docker-01");
        let docker = Arc::new(docker);
        let controller = Controller::new(
            Arc::clone(&docker),
            Arc::clone(&store),
            Arc::clone(&sources),
        );
        Self {
            _directory: directory,
            database_path,
            store,
            sources,
            application,
            docker,
            controller,
        }
    }

    /// A controller over this harness's Docker, store and sources.
    fn new_controller(&self) -> Controller<FakeDocker> {
        Controller::new(
            Arc::clone(&self.docker),
            Arc::clone(&self.store),
            Arc::clone(&self.sources),
        )
    }

    /// Replaces the application.
    fn set_application(&mut self, application: NormalizedApplication) {
        self.application = application;
    }

    /// The application's `production` environment, once created.
    async fn environment(&self) -> EnvironmentId {
        self.production_of(self.application.id()).await
    }

    /// The `production` environment of the application `saved` saved.
    async fn production(&self, saved: &piqueld_core::api::SavedApplication) -> EnvironmentId {
        self.production_of(&ApplicationId::parse(&saved.application_id).unwrap())
            .await
    }

    /// The application environment `environment` belongs to.
    async fn application_of(&self, environment: &EnvironmentId) -> ApplicationId {
        let stored = self.store.get(environment).await.unwrap();
        stored.environment.application_id
    }

    /// The ID of `application`'s `production` environment.
    async fn production_of(&self, application: &ApplicationId) -> EnvironmentId {
        self.store
            .environments(application)
            .await
            .unwrap()
            .into_iter()
            .find(|environment| environment.name.as_str() == "production")
            .expect("a production environment")
            .id
    }

    /// The target the application's environment runs.
    async fn resolved(&self) -> ResolvedApplication {
        self.target(&self.environment().await).await
    }

    /// Gives the controller `timeout` to wait for each convergence.
    fn set_convergence_timeout(&mut self, timeout: Duration) {
        self.set_retry_policy(piqueld::reconcile::RetryPolicy {
            convergence_timeout: timeout,
            ..piqueld::reconcile::RetryPolicy::default()
        });
    }

    /// Replaces the controller with one following `retry`.
    fn set_retry_policy(&mut self, retry: piqueld::reconcile::RetryPolicy) {
        self.controller = self.new_controller().with_retry_policy(retry);
    }

    fn applications(&self) -> TestApplications {
        TestApplications::new(
            Arc::clone(&self.store),
            self.controller
                .runtime(Arc::new(tokio::sync::Notify::new())),
            Arc::clone(&self.sources),
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
                Some(&prebuilt::resolver(
                    &self.application,
                    self.store.instance_id(),
                )),
                None,
            )
            .await
            .expect("application saved")
    }

    /// Saves and deploys application `name` beside the harness's own: the
    /// fixture as `edit` changes it. Returns its environment.
    async fn deploy_application(
        &self,
        name: &str,
        edit: impl FnOnce(&mut ApplicationManifest),
    ) -> EnvironmentId {
        let mut manifest = self.application.to_manifest();
        manifest.metadata.name = name.into();
        edit(&mut manifest);
        let application = manifest
            .validate()
            .unwrap()
            .normalize(ApplicationId::parse(format!("app-{name}-0001")).unwrap());
        self.store
            .save_application(
                &ApplicationTemplate::from(&application),
                Some(&prebuilt::resolver(&application, self.store.instance_id())),
                None,
            )
            .await
            .unwrap()
            .environment_id
    }

    /// Stores a value for the application's secret `token`, replacing
    /// version `generation`.
    async fn store_token(&self, environment: &EnvironmentId, generation: i64, value: &[u8]) {
        self.store
            .put_stored_secret(
                Daemon,
                &self.application_of(environment).await,
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
                &piqueld_store::Visibility::ALL,
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
        self.store = Arc::new(Store::open_unsynced(&self.database_path).await.unwrap());
        let directory = self.database_path.parent().unwrap();
        self.sources = Arc::new(Sources::open(directory).await.unwrap());
        self.controller = self.new_controller();
    }

    async fn assert_recovered(&self, operation_id: &str) {
        let status = self
            .store
            .status(&self.environment().await)
            .await
            .expect("status is readable");
        assert_eq!(status.state, piqueld_store::ApplicationState::Ready);
        let operation = self.operation(operation_id).await;
        assert_eq!(operation.state, OperationState::Succeeded);
        let observed = self
            .docker
            .observe(&self.environment().await)
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
                Some(&prebuilt::resolver(&replacement, self.store.instance_id())),
                None,
            )
            .await
            .expect("application replacement is durable")
    }

    /// The replicas Docker runs of the first service.
    async fn replicas(&self) -> u16 {
        self.replicas_of(&self.environment().await).await
    }

    /// The replicas Docker runs of environment `id`'s first service.
    async fn replicas_of(&self, id: &EnvironmentId) -> u16 {
        self.docker.observe(id).await.unwrap().services[0].replicas
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
            .request_delete(&self.environment().await)
            .await
            .expect("delete is durable")
    }

    /// Asserts `deletion` removed the application's environment and its
    /// resources, except its volume.
    async fn assert_deleted(&self, deletion: &Operation) {
        assert!(matches!(
            self.store.get(&deletion.environment_id).await,
            Err(piqueld_store::StoreError::NotFound)
        ));
        let observed = self
            .docker
            .observe(&deletion.environment_id)
            .await
            .expect("final observation");
        assert_eq!(observed.services, [] as [ObservedService; 0]);
        assert_eq!(observed.networks, [] as [ObservedNetwork; 0]);
        assert_eq!(observed.volumes.len(), 1);
    }

    /// Journals the application, seeds its resources as Docker would run
    /// them, and checks planning then finds nothing to do. Nothing reconciles
    /// before the next scan.
    async fn seed_matching_runtime(&self) -> Operation {
        let created = self.create().await;
        let resolved = self.resolved().await;
        self.docker
            .ensure_network(&resolved.networks[0])
            .await
            .expect("network is seeded");
        self.docker
            .ensure_volume(&resolved.volumes[0])
            .await
            .expect("volume is seeded");
        self.docker
            .ensure_service(&resolved.services[0])
            .await
            .expect("service is seeded");
        let observed = self
            .docker
            .observe(&resolved.id)
            .await
            .expect("matching observation");
        let plan = Plan::from_request(&PlanRequest::Reconcile { desired: resolved }, &observed);
        assert_eq!(plan.actions, [] as [piqueld_core::PlanAction; 0]);
        created
    }

    /// Journals the application over Docker resources another installation
    /// already created, as `foreign` adds them from the application's target,
    /// and asserts one scan journals the conflict as a degraded, failed
    /// operation, leaving them untouched.
    async fn assert_refuses_foreign(
        &self,
        foreign: impl FnOnce(&ResolvedApplication, &mut ObservedApplication),
    ) {
        let created = self.create().await;
        foreign(
            &self.resolved().await,
            &mut *self.docker.observed.lock().await,
        );
        let foreign = self.docker.observed.lock().await.clone();
        self.scan().await;
        let status = self
            .store
            .status(&self.environment().await)
            .await
            .expect("status is readable");
        assert_eq!(status.state, piqueld_store::ApplicationState::Degraded);
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
        let failed = self.apply(migration(), None).await;
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

/// `observed`'s `web` service.
fn web(observed: &ObservedApplication) -> ObservedService {
    observed
        .services
        .iter()
        .find(|service| service.labels.get(SERVICE_LABEL).map(String::as_str) == Some("web"))
        .unwrap()
        .clone()
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

/// Automatic rollback: a failed rollout restores the environment's last
/// successful deployment where `rollback = "automatic"`, which is never
/// rolled back itself, and leaves other environments degraded.
mod automatic_rollback;
mod controller;
mod environments;
/// Image retention: cleanup removes only this installation's unretained
/// builds, never while a preparation may record one, and deployments that
/// use a retained image again find it or fail naming it.
mod image_retention;
mod jobs;
/// Promoting releases into promoted environments, which never build or fetch.
mod promotion;
mod repository_deployments;
mod rollouts;
mod secrets;
mod timeline;
