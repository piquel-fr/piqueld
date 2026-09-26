//! Reconciliation coverage using the real Docker seam and an in-memory backend.

#[path = "support/git.rs"]
mod git_fixture;

#[path = "support/application.rs"]
mod application_fixture;
use application_fixture::TestApplications;

use async_trait::async_trait;
use piqueld::docker::{DockerApi, DockerError, ImageSource, SwarmState, resolve_image_digest};
use piqueld::reconcile::Controller;
use piqueld::store::Store;
use piqueld_core::Sha256Digest;
use piqueld_core::planner::PlanRequest;
use piqueld_core::resource::{
    APPLICATION_LABEL, Convergence, DesiredService, INSTANCE_LABEL, MANAGED_LABEL,
    ObservedApplication, ObservedNetwork, ObservedService, ObservedTask, ObservedVolume,
    ResolutionSet, ResolvedApplication, ResolvedSource, SERVICE_LABEL, SPEC_HASH_LABEL, TaskState,
    compile_application, image_repository,
};
use piqueld_core::{ApplicationId, InstanceId, Plan, parse_toml};
use piqueld_core::{Operation, OperationState};
use sqlx::{Connection, SqliteConnection};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
struct FakeDocker {
    observed: Arc<Mutex<ObservedApplication>>,
    secret_values: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    registry: Arc<Mutex<RegistryState>>,
    deny_network_removal: Arc<AtomicBool>,
    resolution_gate: Option<Arc<ResolutionGate>>,
    isolate_observations: bool,
    mutations: Arc<Probe>,
    images: Arc<Probe>,
    observations: Arc<Probe>,
}

/// Programmatic hook: the tag is re-pointed after this many remaining pulls.
#[derive(Default)]
struct RegistryState {
    pulls: BTreeMap<String, u64>,
    digests: BTreeMap<String, String>,
    flips_remaining: usize,
}

impl RegistryState {
    fn base_digest() -> String {
        "a".repeat(64)
    }

    fn flipped_digest() -> String {
        "b".repeat(64)
    }

    fn digest(&self, reference: &str) -> String {
        self.digests
            .get(reference)
            .cloned()
            .unwrap_or_else(Self::base_digest)
    }

    fn pull(&mut self, reference: &str) {
        *self.pulls.entry(reference.to_owned()).or_insert(0) += 1;
        if self.flips_remaining > 0 {
            self.flips_remaining -= 1;
            let flipped = if self.digest(reference) == Self::base_digest() {
                Self::flipped_digest()
            } else {
                Self::base_digest()
            };
            self.digests.insert(reference.to_owned(), flipped);
        }
    }
}

/// An [`ImageSource`] view over the fake registry, mirroring the real
/// engine's canonical repo digest shape.
struct RegistryView {
    registry: Arc<Mutex<RegistryState>>,
}

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

impl FakeDocker {
    fn with_observed(observed: ObservedApplication) -> Self {
        Self {
            observed: Arc::new(Mutex::new(observed)),
            deny_network_removal: Arc::default(),
            registry: Arc::new(Mutex::new(RegistryState::default())),
            ..Self::default()
        }
    }

    async fn assert_active_repair(&self, id: &ApplicationId) {
        {
            let mut observed = self.observed.lock().await;
            let service = observed
                .services
                .iter_mut()
                .find(|service| service.labels.get(APPLICATION_LABEL) == Some(&id.to_string()))
                .unwrap();
            service.replicas = 9;
            service.convergence = Convergence::Degraded;
        }
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if self.observe(id).await.unwrap().services[0].replicas == 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("active target is repaired while candidate image pull is blocked");
    }

    /// Arms the registry to re-point the tag after each remaining pull.
    async fn arm_tag_flips(&self, flips: usize) {
        self.registry.lock().await.flips_remaining = flips;
    }

    fn ownership_matches(
        observed: &BTreeMap<String, String>,
        expected: &BTreeMap<String, String>,
    ) -> bool {
        let labels_match = [
            MANAGED_LABEL,
            INSTANCE_LABEL,
            APPLICATION_LABEL,
            SERVICE_LABEL,
        ]
        .iter()
        .filter_map(|key| expected.get(*key).map(|value| (*key, value)))
        .all(|(key, value)| observed.get(key) == Some(value));
        let spec_hash_valid = !expected.contains_key(APPLICATION_LABEL)
            || observed
                .get(SPEC_HASH_LABEL)
                .is_some_and(|value| Sha256Digest::parse(value.clone()).is_ok());
        labels_match && spec_hash_valid
    }
}

#[async_trait]
impl ImageSource for RegistryView {
    async fn repo_digests(&self, reference: &str) -> Result<Option<Vec<String>>, DockerError> {
        let state = self.registry.lock().await;
        let repository = image_repository(reference).expect("test references are valid");
        Ok(Some(vec![format!(
            "{repository}@sha256:{}",
            state.digest(reference)
        )]))
    }

    async fn pull(&self, reference: &str) -> Result<(), DockerError> {
        self.registry.lock().await.pull(reference);
        Ok(())
    }
}

fn observed_service(desired: &DesiredService) -> ObservedService {
    ObservedService {
        secrets: desired.secrets.clone(),
        name: desired.name.to_string(),
        image: desired.image.to_string(),
        replicas: desired.replicas,
        environment: desired.environment.clone(),
        command: desired.command.clone(),
        arguments: desired.arguments.clone(),
        mounts: desired
            .mounts
            .iter()
            .map(piqueld_core::resource::ObservedMount::from)
            .collect(),
        healthcheck: desired.healthcheck.clone(),
        healthcheck_configured: desired.healthcheck.is_some(),
        resources: desired.resources.clone(),
        networks: desired.networks.iter().map(ToString::to_string).collect(),
        labels: desired.labels.clone(),
        runtime_configuration_matches: true,
        tasks: vec![ObservedTask {
            state: TaskState::Running,
            healthy: Some(true),
            desired_running: true,
            diagnostic: None,
        }],
        convergence: Convergence::Converged,
    }
}

#[async_trait]
impl DockerApi for FakeDocker {
    async fn ensure_secret(
        &self,
        name: &str,
        value: &[u8],
        _ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        self.secret_values
            .lock()
            .await
            .entry(name.to_owned())
            .or_insert_with(|| value.to_vec());
        Ok(())
    }

    async fn remove_secrets(
        &self,
        names: &[String],
        _ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        let mut values = self.secret_values.lock().await;
        for name in names {
            values.remove(name);
        }
        Ok(())
    }

    async fn application_logs(
        &self,
        _instance: &InstanceId,
        _application: &ApplicationId,
        _service: Option<&str>,
        _tail: u16,
        _since: u32,
        _stream: Option<piqueld_core::api::LogStream>,
    ) -> Result<piqueld_core::api::ApplicationLogs, DockerError> {
        Ok(piqueld_core::api::ApplicationLogs::default())
    }

    async fn ping(&self) -> Result<(), DockerError> {
        Ok(())
    }

    async fn ensure_swarm(&self, _auto_initialize: bool) -> Result<SwarmState, DockerError> {
        Ok(SwarmState::Ready)
    }

    async fn build_image(
        &self,
        dockerfile: &std::path::Path,
        context: &std::path::Path,
    ) -> Result<Sha256Digest, DockerError> {
        assert!(context.is_dir());
        let contents = tokio::fs::read_to_string(dockerfile).await.unwrap();
        if contents.contains("build-fails") {
            return Err(DockerError::Request("build Docker image"));
        }
        self.registry.lock().await.pull("git-build");
        Ok(Sha256Digest::parse(format!("sha256:{}", "b".repeat(64))).unwrap())
    }
    async fn resolve_image(&self, reference: &str) -> Result<String, DockerError> {
        let _probe = self.images.enter().await;
        if reference.contains("/slow:")
            && let Some(gate) = &self.resolution_gate
        {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        resolve_image_digest(
            &RegistryView {
                registry: Arc::clone(&self.registry),
            },
            reference,
        )
        .await
    }

    async fn observe(
        &self,
        application: &ApplicationId,
    ) -> Result<ObservedApplication, DockerError> {
        let _probe = self.observations.enter().await;
        let mut observed = self.observed.lock().await.clone();
        if self.isolate_observations {
            let belongs = |labels: &BTreeMap<String, String>| {
                labels
                    .get(APPLICATION_LABEL)
                    .is_some_and(|id| id == application.as_str())
            };
            observed.services.retain(|service| belongs(&service.labels));
            observed.networks.retain(|network| belongs(&network.labels));
            observed.volumes.retain(|volume| belongs(&volume.labels));
        }
        Ok(observed)
    }

    async fn ensure_network(
        &self,
        desired: &piqueld_core::resource::DesiredNetwork,
    ) -> Result<(), DockerError> {
        let _probe = self.mutations.enter().await;
        let mut observed = self.observed.lock().await;
        if let Some(existing) = observed
            .networks
            .iter()
            .find(|network| network.name == desired.name.as_str())
        {
            if !Self::ownership_matches(&existing.labels, &desired.labels)
                || existing.labels.contains_key(SERVICE_LABEL)
            {
                return Err(DockerError::OwnershipConflict);
            }
            if !existing.runtime_configuration_matches {
                return Err(DockerError::ConfigurationConflict);
            }
            return Ok(());
        }
        observed.networks.push(ObservedNetwork {
            name: desired.name.to_string(),
            runtime_configuration_matches: true,
            labels: desired.labels.clone(),
        });
        Ok(())
    }

    async fn ensure_volume(
        &self,
        desired: &piqueld_core::resource::DesiredVolume,
    ) -> Result<(), DockerError> {
        let _probe = self.mutations.enter().await;
        let mut observed = self.observed.lock().await;
        if let Some(existing) = observed
            .volumes
            .iter()
            .find(|volume| volume.name == desired.name.as_str())
        {
            if !Self::ownership_matches(&existing.labels, &desired.labels)
                || existing.labels.contains_key(SERVICE_LABEL)
            {
                return Err(DockerError::OwnershipConflict);
            }
            if !existing.runtime_configuration_matches {
                return Err(DockerError::ConfigurationConflict);
            }
            return Ok(());
        }
        observed.volumes.push(ObservedVolume {
            name: desired.name.to_string(),
            runtime_configuration_matches: true,
            labels: desired.labels.clone(),
        });
        Ok(())
    }

    async fn ensure_service(
        &self,
        desired: &piqueld_core::resource::DesiredService,
    ) -> Result<(), DockerError> {
        let _probe = self.mutations.enter().await;
        let mut observed = self.observed.lock().await;
        if let Some(existing) = observed
            .services
            .iter()
            .find(|service| service.name == desired.name.as_str())
            && !Self::ownership_matches(&existing.labels, &desired.labels)
        {
            return Err(DockerError::OwnershipConflict);
        }
        observed
            .services
            .retain(|service| service.name != desired.name.as_str());
        observed.services.push(observed_service(desired));
        Ok(())
    }

    async fn remove_service(
        &self,
        name: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        let _probe = self.mutations.enter().await;
        let mut observed = self.observed.lock().await;
        if let Some(existing) = observed
            .services
            .iter()
            .find(|service| service.name == name)
            && !Self::ownership_matches(&existing.labels, ownership)
        {
            return Err(DockerError::OwnershipConflict);
        }
        observed.services.retain(|service| service.name != name);
        Ok(())
    }

    async fn remove_network(
        &self,
        name: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        if self.deny_network_removal.load(Ordering::Relaxed) {
            return Err(DockerError::OwnershipConflict);
        }
        let _probe = self.mutations.enter().await;
        let mut observed = self.observed.lock().await;
        if let Some(existing) = observed
            .networks
            .iter()
            .find(|network| network.name == name)
            && !Self::ownership_matches(&existing.labels, ownership)
        {
            return Err(DockerError::OwnershipConflict);
        }
        observed.networks.retain(|network| network.name != name);
        Ok(())
    }
}

fn application() -> piqueld_core::NormalizedApplication {
    parse_toml(include_str!(
        "../../../crates/piqueld-core/tests/fixtures/manifests/prebuilt.toml"
    ))
    .expect("fixture is valid")
    .normalize(ApplicationId::parse("app-fake-docker-01").expect("valid application ID"))
}

async fn fixture_store(
    directory: &tempfile::TempDir,
) -> (
    Arc<Store>,
    piqueld_core::NormalizedApplication,
    ResolvedApplication,
) {
    let store = Arc::new(
        Store::open(directory.path().join("control-plane.db"))
            .await
            .expect("fresh database opens"),
    );
    let application = application();
    let resolutions = ResolutionSet {
        secret_names: std::collections::BTreeMap::default(),
        sources: [(
            piqueld_core::ServiceName::parse("web").unwrap(),
            ResolvedSource::parse_image(
                "ghcr.io/example/notes:1.4.0",
                format!("ghcr.io/example/notes@sha256:{}", "a".repeat(64)),
            )
            .unwrap(),
        )]
        .into_iter()
        .collect(),
    };
    let resolved = compile_application(
        &application,
        InstanceId::parse(store.instance_id()).expect("store instance ID is valid"),
        &resolutions,
    )
    .expect("fixture resolves");
    (store, application, resolved)
}

fn foreign_labels(application_id: &ApplicationId) -> BTreeMap<String, String> {
    BTreeMap::from([
        (MANAGED_LABEL.into(), "true".into()),
        (INSTANCE_LABEL.into(), "other-instance".into()),
        (APPLICATION_LABEL.into(), application_id.to_string()),
        (SPEC_HASH_LABEL.into(), format!("sha256:{}", "b".repeat(64))),
    ])
}

struct ControllerHarness {
    _directory: tempfile::TempDir,
    database_path: PathBuf,
    store: Arc<Store>,
    application: piqueld_core::NormalizedApplication,
    resolutions: ResolutionSet,
    resolved: ResolvedApplication,
    docker: Arc<FakeDocker>,
    controller: Controller<FakeDocker>,
}

impl ControllerHarness {
    async fn new() -> Self {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database_path = directory.path().join("control-plane.db");
        let store = Arc::new(
            Store::open(&database_path)
                .await
                .expect("fresh database opens"),
        );
        let application = application();
        let resolutions = ResolutionSet {
            secret_names: std::collections::BTreeMap::default(),
            sources: [(
                piqueld_core::ServiceName::parse("web").unwrap(),
                ResolvedSource::parse_image(
                    "ghcr.io/example/notes:1.4.0",
                    format!("ghcr.io/example/notes@sha256:{}", "a".repeat(64)),
                )
                .unwrap(),
            )]
            .into_iter()
            .collect(),
        };
        let resolved = compile_application(
            &application,
            InstanceId::parse(store.instance_id()).expect("store instance ID is valid"),
            &resolutions,
        )
        .expect("fixture resolves");
        let docker = Arc::new(FakeDocker::default());
        let controller = Controller::new(Arc::clone(&docker), Arc::clone(&store));
        Self {
            _directory: directory,
            database_path,
            store,
            application,
            resolutions,
            resolved,
            docker,
            controller,
        }
    }

    fn reconcile_plan(
        desired: ResolvedApplication,
        observed: &ObservedApplication,
    ) -> piqueld_core::Plan {
        Plan::from_request(&PlanRequest::Reconcile { desired }, observed)
    }

    async fn create(&self) -> Operation {
        self.store
            .save_application(&self.application, Some(&self.resolved), None)
            .await
            .expect("application saved")
    }

    async fn interrupt(&self, operation_id: &str) {
        let mut connection = SqliteConnection::connect(&format!(
            "sqlite://{}?mode=rwc",
            self.database_path.display()
        ))
        .await
        .expect("database can be inspected");
        sqlx::query(
            "UPDATE operations SET state='running',started_at_ms=created_at_ms,updated_at_ms=created_at_ms WHERE id=?1",
        )
        .bind(operation_id)
        .execute(&mut connection)
        .await
        .expect("operation can be interrupted");
    }

    async fn assert_recovered(&self, operation_id: &str) {
        let status = self
            .store
            .status(self.application.id())
            .await
            .expect("status is readable");
        assert_eq!(status.state, piqueld::store::ApplicationState::Ready);
        let operation = self
            .store
            .operation(operation_id)
            .await
            .expect("operation journal is readable");
        assert_eq!(operation.state, OperationState::Succeeded);
        let observed = self
            .docker
            .observe(self.application.id())
            .await
            .expect("fake observation");
        assert_eq!(observed.volumes.len(), 1);
        assert_eq!(observed.services.len(), 1);
    }

    async fn replace(&self) -> (Operation, ResolvedApplication) {
        let mut replacement = self.application.to_manifest();
        replacement.spec.services[0].replicas = 2;
        let replacement = replacement
            .validate()
            .unwrap()
            .normalize(self.application.id().clone());
        let replacement_resolved = compile_application(
            &replacement,
            InstanceId::parse(self.store.instance_id()).expect("store instance ID is valid"),
            &self.resolutions,
        )
        .expect("replacement resolves");
        let replaced = self
            .store
            .save_application(&replacement, Some(&replacement_resolved), None)
            .await
            .expect("application replacement is durable");
        (replaced, replacement_resolved)
    }

    async fn repair_drift(&self) {
        {
            let mut observed = self.docker.observed.lock().await;
            observed.services[0].replicas = 1;
            observed.services[0].convergence = Convergence::Degraded;
        }
        self.controller
            .scan(&CancellationToken::new())
            .await
            .expect("drift repair converges");
        assert_eq!(
            self.docker
                .observe(self.application.id())
                .await
                .unwrap()
                .services[0]
                .replicas,
            2
        );
    }

    async fn delete(&self) -> Operation {
        self.store
            .request_delete(self.application.id(), None)
            .await
            .expect("delete is durable")
    }

    async fn assert_deleted(&self) {
        assert!(matches!(
            self.store.get(self.application.id()).await,
            Err(piqueld::store::StoreError::NotFound)
        ));
        let observed = self
            .docker
            .observe(self.application.id())
            .await
            .expect("final observation");
        assert!(observed.services.is_empty());
        assert!(observed.networks.is_empty());
        assert_eq!(observed.volumes.len(), 1);
    }
}

#[tokio::test]
async fn controller_converges_a_prebuilt_application_through_the_docker_seam() {
    let harness = ControllerHarness::new().await;
    let created = harness.create().await;
    harness.interrupt(&created.id).await;
    harness
        .controller
        .scan(&CancellationToken::new())
        .await
        .expect("controller converges the operation");
    harness.assert_recovered(&created.id).await;

    harness.replace().await;
    harness
        .controller
        .scan(&CancellationToken::new())
        .await
        .expect("replacement converges");
    assert_eq!(
        harness
            .docker
            .observe(harness.application.id())
            .await
            .unwrap()
            .services[0]
            .replicas,
        2
    );

    harness
        .controller
        .scan(&CancellationToken::new())
        .await
        .expect("matching state is idempotent");
    harness.repair_drift().await;
    let deletion = harness.delete().await;
    harness
        .docker
        .deny_network_removal
        .store(true, Ordering::Relaxed);
    harness
        .controller
        .scan(&CancellationToken::new())
        .await
        .unwrap();
    let blocked = harness.store.operation(&deletion.id).await.unwrap();
    assert_eq!(blocked.state, OperationState::Running);
    assert_eq!(blocked.error_code.as_deref(), Some("ownership_conflict"));
    assert!(blocked.finished_at_ms.is_none());
    assert!(
        harness
            .store
            .get(harness.application.id())
            .await
            .unwrap()
            .delete_intent
    );
    harness
        .docker
        .deny_network_removal
        .store(false, Ordering::Relaxed);
    harness
        .controller
        .scan(&CancellationToken::new())
        .await
        .expect("delete converges");
    harness.assert_deleted().await;
    assert!(matches!(
        harness.store.operation(&deletion.id).await,
        Err(piqueld::store::StoreError::NotFound)
    ));
}

#[tokio::test]
async fn controller_executes_actions_introduced_by_fresh_planning() {
    let harness = ControllerHarness::new().await;
    harness
        .docker
        .ensure_network(&harness.resolved.networks[0])
        .await
        .expect("network is seeded");
    harness
        .docker
        .ensure_volume(&harness.resolved.volumes[0])
        .await
        .expect("volume is seeded");
    harness
        .docker
        .ensure_service(&harness.resolved.services[0])
        .await
        .expect("service is seeded");
    let observed = harness
        .docker
        .observe(harness.application.id())
        .await
        .expect("matching observation");
    let plan = ControllerHarness::reconcile_plan(harness.resolved.clone(), &observed);
    assert!(plan.actions.is_empty());
    harness
        .store
        .save_application(&harness.application, Some(&harness.resolved), None)
        .await
        .expect("matching application is journaled");

    harness.docker.observed.lock().await.networks.clear();
    harness
        .controller
        .scan(&CancellationToken::new())
        .await
        .expect("fresh action converges");

    let status = harness
        .store
        .status(harness.application.id())
        .await
        .unwrap();
    assert_eq!(status.state, piqueld::store::ApplicationState::Ready);
    assert_eq!(harness.docker.observed.lock().await.networks.len(), 1);
}

#[tokio::test]
async fn superseded_operations_do_not_plan_stale_runtime_state() {
    let harness = ControllerHarness::new().await;
    harness
        .docker
        .ensure_network(&harness.resolved.networks[0])
        .await
        .expect("network is seeded");
    harness
        .docker
        .ensure_volume(&harness.resolved.volumes[0])
        .await
        .expect("volume is seeded");
    harness
        .docker
        .ensure_service(&harness.resolved.services[0])
        .await
        .expect("service is seeded");
    let observed = harness
        .docker
        .observe(harness.application.id())
        .await
        .expect("matching observation");
    let plan = ControllerHarness::reconcile_plan(harness.resolved.clone(), &observed);
    assert!(plan.actions.is_empty());
    let stale = harness
        .store
        .save_application(&harness.application, Some(&harness.resolved), None)
        .await
        .expect("matching application is journaled");

    harness.docker.observed.lock().await.networks.clear();
    let (replacement, _) = harness.replace().await;
    harness
        .controller
        .scan(&CancellationToken::new())
        .await
        .expect("the latest desired state converges");

    let stale_operation = harness
        .store
        .operation(&stale.id)
        .await
        .expect("superseded operation is readable");
    assert_eq!(stale_operation.state, OperationState::Superseded);
    let replacement_operation = harness
        .store
        .operation(&replacement.id)
        .await
        .expect("replacement operation is readable");
    assert_eq!(replacement_operation.state, OperationState::Succeeded);
}

#[tokio::test]
async fn controller_refuses_a_foreign_same_name_service() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let (store, application, resolved) = fixture_store(&directory).await;
    let mut foreign_service_labels = foreign_labels(application.id());
    foreign_service_labels.insert(SERVICE_LABEL.into(), "web".into());
    let foreign = ObservedService {
        labels: foreign_service_labels,
        ..observed_service(&resolved.services[0])
    };
    let created = store
        .save_application(&application, Some(&resolved), None)
        .await
        .expect("application is created");
    let docker = Arc::new(FakeDocker::with_observed(ObservedApplication {
        services: vec![foreign],
        ..ObservedApplication::default()
    }));
    let controller = Controller::new(Arc::clone(&docker), Arc::clone(&store));
    controller
        .scan(&CancellationToken::new())
        .await
        .expect("ownership conflict is journaled");
    let status = store
        .status(application.id())
        .await
        .expect("status is readable");
    assert_eq!(status.state, piqueld::store::ApplicationState::Degraded);
    let operation = store
        .operation(&created.id)
        .await
        .expect("failed operation is readable");
    assert_eq!(operation.state, OperationState::Failed);
    assert_eq!(
        docker
            .observe(application.id())
            .await
            .unwrap()
            .services
            .len(),
        1
    );
}

/// Runs one reconciliation against a pre-seeded foreign fixture and asserts the
/// conflict is journaled as a degraded, failed operation.
async fn assert_foreign_fixture_refuses_reconciliation(
    docker: &Arc<FakeDocker>,
    store: &Arc<Store>,
    application: &piqueld_core::NormalizedApplication,
    resolved: &ResolvedApplication,
) -> Operation {
    let created = store
        .save_application(application, Some(resolved), None)
        .await
        .expect("application is created");
    let controller = Controller::new(Arc::clone(docker), Arc::clone(store));
    controller
        .scan(&CancellationToken::new())
        .await
        .expect("ownership conflict is journaled");
    let status = store
        .status(application.id())
        .await
        .expect("status is readable");
    assert_eq!(status.state, piqueld::store::ApplicationState::Degraded);
    let operation = store
        .operation(&created.id)
        .await
        .expect("failed operation is readable");
    assert_eq!(operation.state, OperationState::Failed);
    created
}

#[tokio::test]
async fn controller_refuses_a_foreign_same_name_network() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let (store, application, resolved) = fixture_store(&directory).await;
    let foreign = ObservedNetwork {
        name: resolved.networks[0].name.to_string(),
        runtime_configuration_matches: true,
        labels: foreign_labels(application.id()),
    };
    let docker = Arc::new(FakeDocker::with_observed(ObservedApplication {
        networks: vec![foreign],
        ..ObservedApplication::default()
    }));
    assert_foreign_fixture_refuses_reconciliation(&docker, &store, &application, &resolved).await;
    // The foreign network must survive untouched.
    let observed = docker.observe(application.id()).await.unwrap();
    assert_eq!(observed.networks.len(), 1);
    assert_eq!(
        observed.networks[0].labels.get(INSTANCE_LABEL),
        Some(&"other-instance".to_string())
    );
}

#[tokio::test]
async fn controller_refuses_a_foreign_same_name_volume() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let (store, application, resolved) = fixture_store(&directory).await;
    let foreign = ObservedVolume {
        name: resolved.volumes[0].name.to_string(),
        runtime_configuration_matches: true,
        labels: foreign_labels(application.id()),
    };
    let docker = Arc::new(FakeDocker::with_observed(ObservedApplication {
        volumes: vec![foreign],
        ..ObservedApplication::default()
    }));
    assert_foreign_fixture_refuses_reconciliation(&docker, &store, &application, &resolved).await;
    // The foreign volume must survive untouched.
    let observed = docker.observe(application.id()).await.unwrap();
    assert_eq!(observed.volumes.len(), 1);
    assert_eq!(
        observed.volumes[0].labels.get(INSTANCE_LABEL),
        Some(&"other-instance".to_string())
    );
}

#[tokio::test]
async fn image_resolution_repairs_a_single_tag_flip_through_a_retry() {
    let docker = FakeDocker::default();
    docker.arm_tag_flips(1).await;

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

#[derive(Default)]
struct ResolutionGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[derive(Default)]
struct Probe {
    active: std::sync::atomic::AtomicUsize,
    maximum: std::sync::atomic::AtomicUsize,
}
struct ProbeGuard<'a>(&'a Probe);
impl Drop for ProbeGuard<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Probe {
    async fn enter(&self) -> ProbeGuard<'_> {
        let guard = ProbeGuard(self);
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(active, Ordering::SeqCst);
        tokio::task::yield_now().await;
        guard
    }
}

impl ControllerHarness {
    fn applications(&self) -> TestApplications {
        TestApplications::new(
            Arc::clone(&self.store),
            self.controller
                .runtime(Arc::new(tokio::sync::Notify::new())),
        )
    }

    async fn finish(&self, operation: &Operation) {
        self.controller
            .scan(&CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            self.store.operation(&operation.id).await.unwrap().state,
            OperationState::Succeeded
        );
    }

    async fn target(&self, id: &ApplicationId) -> ResolvedApplication {
        self.store.get(id).await.unwrap().resolved.unwrap()
    }

    async fn pulls(&self) -> u64 {
        self.docker.registry.lock().await.pulls.values().sum()
    }
}

#[tokio::test]
async fn apply_is_durable_before_resolution_and_deploy_is_explicit() {
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let manifest = manifest();
    let accepted = applications
        .apply(manifest.clone().validate().unwrap(), Some(0))
        .await
        .unwrap();
    assert_eq!(harness.pulls().await, 0);
    let stored = harness.store.get(&accepted.application_id).await.unwrap();
    assert_eq!(stored.generation, 1);
    assert!(stored.resolved.is_none());
    harness.finish(&accepted).await;
    let pulls = harness.pulls().await;
    let repeated = applications
        .apply(manifest.clone().validate().unwrap(), Some(1))
        .await
        .unwrap();
    assert_eq!(repeated.id, accepted.id);
    assert_eq!(harness.pulls().await, pulls);
    let reconcile = applications
        .reconcile(&accepted.application_id, Some(1))
        .await
        .unwrap();
    harness.finish(&reconcile).await;
    assert_eq!(harness.pulls().await, pulls);
    let refresh = applications
        .deploy(&accepted.application_id, Some(1))
        .await
        .unwrap();
    assert_ne!(refresh.id, accepted.id);
    harness.finish(&refresh).await;
    assert!(harness.pulls().await > pulls);
    assert_eq!(
        harness
            .store
            .get(&accepted.application_id)
            .await
            .unwrap()
            .generation,
        1
    );
    let events = harness
        .store
        .events(Some(&accepted.application_id), None, 100)
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
    let first = applications
        .apply(manifest.clone().validate().unwrap(), Some(0))
        .await
        .unwrap();
    manifest.spec.services[0]
        .environment
        .insert("CHANGED".into(), "yes".into());
    let second = applications
        .apply(manifest.clone().validate().unwrap(), Some(1))
        .await
        .unwrap();
    assert_eq!(second.generation, 2);
    assert!(matches!(
        applications
            .apply(manifest.clone().validate().unwrap(), Some(1))
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
            .delete(&first.application_id, Some(1))
            .await
            .is_err()
    );
    manifest.spec.services[0].environment.clear();
    let third = applications
        .apply(manifest.clone().validate().unwrap(), Some(2))
        .await
        .unwrap();
    assert!(
        harness
            .store
            .get(&first.application_id)
            .await
            .unwrap()
            .application
            .spec()
            .services[0]
            .environment
            .is_empty()
    );
    assert_eq!(third.generation, 3);
    let deletion = applications
        .delete(&first.application_id, Some(3))
        .await
        .unwrap();
    assert_eq!(deletion.generation, 4);
    assert_eq!(
        applications
            .delete(&first.application_id, None)
            .await
            .unwrap()
            .id,
        deletion.id
    );
    assert!(
        applications
            .deploy(&first.application_id, None)
            .await
            .is_err()
    );
    let restored = applications
        .apply(manifest.validate().unwrap(), Some(4))
        .await
        .unwrap();
    assert_eq!(restored.generation, 5);
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
    // Move the persisted failure beyond its backoff without sleeping in the test.
    let mut connection =
        SqliteConnection::connect(&format!("sqlite://{}", harness.database_path.display()))
            .await
            .unwrap();
    sqlx::query("UPDATE operations SET updated_at_ms=1 WHERE id=?1")
        .bind(&operation.id)
        .execute(&mut connection)
        .await
        .unwrap();
    harness.finish(&operation).await;
    assert_eq!(harness.pulls().await, 0);
    let completed = harness.store.operation(&operation.id).await.unwrap();
    assert_eq!(completed.attempt, 2);
    let observed = harness
        .docker
        .observe(&operation.application_id)
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
async fn pending_pulls_do_not_block_other_apps_and_superseded_preparation_is_discarded() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        Store::open(directory.path().join("state.db"))
            .await
            .unwrap(),
    );
    let gate = Arc::new(ResolutionGate::default());
    let docker = Arc::new(FakeDocker {
        resolution_gate: Some(Arc::clone(&gate)),
        isolate_observations: true,
        ..FakeDocker::default()
    });
    let controller = Controller::new(Arc::clone(&docker), Arc::clone(&store));
    let wake = Arc::new(tokio::sync::Notify::new());
    let applications =
        TestApplications::new(Arc::clone(&store), controller.runtime(Arc::clone(&wake)));
    let mut slow = manifest();
    slow.metadata.name = "slow".into();
    let original = applications
        .apply(slow.clone().validate().unwrap(), None)
        .await
        .unwrap();
    controller.scan(&CancellationToken::new()).await.unwrap();
    let original_target = store
        .get(&original.application_id)
        .await
        .unwrap()
        .resolved
        .unwrap();
    slow.spec.services[0].source = piqueld_core::Source::Image {
        image: "ghcr.io/example/slow:1".into(),
    };
    let accepted = applications
        .apply(slow.validate().unwrap(), None)
        .await
        .unwrap();
    let cancellation = CancellationToken::new();
    let token = cancellation.clone();
    let controller_task = tokio::spawn(async move {
        controller
            .run(wake, std::time::Duration::from_millis(50), 10, 30, token)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), gate.entered.notified())
        .await
        .expect("pull started");
    docker.assert_active_repair(&accepted.application_id).await;
    let mut fast = manifest();
    fast.metadata.name = "fast".into();
    let fast = applications
        .apply(fast.validate().unwrap(), None)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if store.operation(&fast.id).await.unwrap().state == OperationState::Succeeded {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("unrelated application completes while slow pull is pending");
    let pending = store.get(&accepted.application_id).await.unwrap();
    assert_eq!(pending.generation, 2);
    assert_eq!(pending.resolved_generation, Some(1));
    assert_eq!(pending.resolved, Some(original_target));
    assert_eq!(
        docker
            .observe(&accepted.application_id)
            .await
            .unwrap()
            .services[0]
            .convergence,
        Convergence::Converged
    );
    let deleted = applications
        .delete(&accepted.application_id, None)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if matches!(
                store.operation(&deleted.id).await,
                Err(piqueld::store::StoreError::NotFound)
            ) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("delete supersedes pending preparation without waiting for its pull");
    assert_eq!(docker.images.maximum.load(Ordering::SeqCst), 2);
    cancellation.cancel();
    controller_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn controller_enforces_global_io_bounds_on_a_single_thread() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        Store::open(directory.path().join("state.db"))
            .await
            .unwrap(),
    );
    let docker = Arc::new(FakeDocker {
        isolate_observations: true,
        ..FakeDocker::default()
    });
    let controller = Controller::new(Arc::clone(&docker), Arc::clone(&store));
    let applications = TestApplications::new(
        Arc::clone(&store),
        controller.runtime(Arc::new(tokio::sync::Notify::new())),
    );
    for index in 0..12 {
        let mut manifest = manifest();
        manifest.metadata.name = format!("app-{index}");
        applications
            .apply(manifest.validate().unwrap(), None)
            .await
            .unwrap();
    }
    controller.scan(&CancellationToken::new()).await.unwrap();
    assert_eq!(docker.mutations.maximum.load(Ordering::SeqCst), 1);
    assert!((1..=2).contains(&docker.images.maximum.load(Ordering::SeqCst)));
    assert!((1..=8).contains(&docker.observations.maximum.load(Ordering::SeqCst)));
    for app in store.list(None, 100).await.unwrap().items {
        assert_eq!(
            store
                .latest_operation_for_application(app.application.id())
                .await
                .unwrap()
                .unwrap()
                .state,
            OperationState::Succeeded
        );
    }
}

fn manifest() -> piqueld_core::manifest::ApplicationManifest {
    toml::from_str(include_str!(
        "../../../crates/piqueld-core/tests/fixtures/manifests/prebuilt.toml"
    ))
    .unwrap()
}

#[tokio::test]
async fn configuration_changes_reuse_active_images_and_rename_preserves_resources() {
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let mut input = manifest();
    let first = applications
        .apply(input.clone().validate().unwrap(), Some(0))
        .await
        .unwrap();
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
        .insert(image.clone(), "b".repeat(64));
    input.spec.services[0].replicas = 2;
    input.spec.services[0]
        .environment
        .insert("TOKEN".into(), "private-value".into());
    let changed = applications
        .apply(input.clone().validate().unwrap(), Some(1))
        .await
        .unwrap();
    harness.finish(&changed).await;
    assert_eq!(harness.pulls().await, pulls);
    let app = harness.store.get(&first.application_id).await.unwrap();
    assert!(
        app.resolved.unwrap().services[0]
            .image
            .as_str()
            .ends_with(&"a".repeat(64))
    );
    let refreshed = applications
        .deploy(&first.application_id, Some(2))
        .await
        .unwrap();
    harness.finish(&refreshed).await;
    let before = harness
        .store
        .get(&first.application_id)
        .await
        .unwrap()
        .resolved
        .unwrap();
    assert!(before.services[0].image.as_str().ends_with(&"b".repeat(64)));
    let renamed = applications
        .accept(
            piqueld::api::Mutation::Rename {
                id: first.application_id.clone(),
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
        .get(&first.application_id)
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
            .latest_operation_for_application(&first.application_id)
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
async fn failed_preparation_preserves_active_repair_and_identical_apply_does_not_retry() {
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let mut input = manifest();
    let first = applications
        .apply(input.clone().validate().unwrap(), None)
        .await
        .unwrap();
    harness.finish(&first).await;
    input.spec.services[0].source = piqueld_core::Source::Image {
        image: "ghcr.io/example/unstable:1".into(),
    };
    harness.docker.arm_tag_flips(100).await;
    let failed = applications
        .apply(input.clone().validate().unwrap(), None)
        .await
        .unwrap();
    harness
        .controller
        .scan(&CancellationToken::new())
        .await
        .unwrap();
    let failure = harness.store.operation(&failed.id).await.unwrap();
    assert_eq!(failure.state, OperationState::Failed);
    assert_eq!(failure.phase.as_deref(), Some("resolving_image"));
    assert_eq!(failure.resource.as_deref(), Some("web"));
    let pulls = harness.pulls().await;
    let duplicate = applications
        .apply(input.validate().unwrap(), None)
        .await
        .unwrap();
    assert_eq!(duplicate.state, OperationState::Failed);
    assert_eq!(duplicate.attempt, failure.attempt);
    assert_eq!(harness.pulls().await, pulls);
    {
        let mut observed = harness.docker.observed.lock().await;
        observed.services[0].replicas = 9;
        observed.services[0].convergence = Convergence::Degraded;
    }
    harness
        .controller
        .scan(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(harness.docker.observed.lock().await.services[0].replicas, 1);
    let stored = harness.store.get(&first.application_id).await.unwrap();
    assert_eq!(stored.resolved_generation, Some(1));
    let events = harness.store.events(None, None, 100).await.unwrap().items;
    assert!(events.iter().any(|event| event.kind == "operation_failed"
        && event.error_code.as_deref() == Some("image_resolution_failed")
        && event.resource.as_deref() == Some("web")));
}

#[tokio::test]
async fn git_deploy_prepares_before_rollout_and_supersedes_pending_requests() {
    use piqueld::api::{Mutation, MutationResponse};
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let mut input = manifest();
    let fixture = git_fixture::GitBuildFixture::new();
    input.spec.services[0].source = fixture.source.clone();
    let first = applications
        .apply(input.clone().validate().unwrap(), Some(0))
        .await
        .unwrap();
    let deploy = || Mutation::Deploy {
        id: first.application_id.clone(),
    };
    let MutationResponse::Operation(replacement) = applications
        .accept(deploy(), None, true, None)
        .await
        .unwrap()
    else {
        panic!("deployment")
    };
    assert_eq!(
        harness.store.operation(&first.id).await.unwrap().state,
        OperationState::Superseded
    );
    let replacement = harness
        .store
        .operation(&replacement.operation_id)
        .await
        .unwrap();
    harness.finish(&replacement).await;
    let active = harness
        .store
        .get(&first.application_id)
        .await
        .unwrap()
        .resolved
        .unwrap();
    assert!(
        matches!(&active.services[0].source, ResolvedSource::Git { commit, .. } if piqueld_core::manifest::valid_git_commit(commit))
    );
    let pulls = harness.pulls().await;
    let MutationResponse::Operation(accepted) = applications
        .accept(deploy(), None, true, Some("git-deploy"))
        .await
        .unwrap()
    else {
        panic!("expected operation")
    };
    let MutationResponse::Operation(replay) = applications
        .accept(deploy(), None, true, Some("git-deploy"))
        .await
        .unwrap()
    else {
        panic!("expected operation")
    };
    assert_eq!(accepted.operation_id, replay.operation_id);
    let operation = harness
        .store
        .operation(&accepted.operation_id)
        .await
        .unwrap();
    harness.finish(&operation).await;
    assert!(harness.pulls().await > pulls);
    let piqueld_core::Source::Git { build, .. } = &mut input.spec.services[0].source else {
        unreachable!()
    };
    let piqueld_core::manifest::Build::Docker { dockerfile, .. } = build;
    *dockerfile = "Failfile".into();
    let failed = applications
        .apply(input.validate().unwrap(), Some(1))
        .await
        .unwrap();
    harness
        .controller
        .scan(&CancellationToken::new())
        .await
        .unwrap();
    let failure = harness.store.operation(&failed.id).await.unwrap();
    assert_eq!(failure.state, OperationState::Failed);
    assert_eq!(failure.phase.as_deref(), Some("building_git"));
    assert_eq!(failure.resource.as_deref(), Some("web"));
    assert_eq!(failure.error_code.as_deref(), Some("git_build_failed"));
    assert_eq!(
        harness
            .store
            .get(&first.application_id)
            .await
            .unwrap()
            .resolved
            .unwrap(),
        active
    );
    harness
        .assert_git_build_history(&first.application_id)
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
        git_service.source = fixture.source.clone();
        if git_fails {
            let piqueld_core::Source::Git { build, .. } = &mut git_service.source else {
                unreachable!()
            };
            let piqueld_core::manifest::Build::Docker { dockerfile, .. } = build;
            *dockerfile = "Failfile".into();
        } else {
            input.spec.services[0].source = piqueld_core::Source::Image {
                image: "ghcr.io/example/unstable:1".into(),
            };
            harness.docker.arm_tag_flips(100).await;
        }
        input.spec.services.push(git_service);
        let operation = harness
            .applications()
            .apply(input.validate().unwrap(), Some(0))
            .await
            .unwrap();
        harness
            .controller
            .scan(&CancellationToken::new())
            .await
            .unwrap();
        let failure = harness.store.operation(&operation.id).await.unwrap();
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
    use piqueld::api::{ApplicationError, Mutation, MutationResponse};
    use piqueld::store::StoreError;
    use piqueld_core::manifest::{ApplicationManifest, GitRepository, RepositoryManifest};

    impl git_fixture::GitBuildFixture {
        pub fn failing_source(&self) -> piqueld_core::Source {
            let mut source = self.source.clone();
            let piqueld_core::Source::Git { build, .. } = &mut source else {
                unreachable!()
            };
            let piqueld_core::manifest::Build::Docker { dockerfile, .. } = build;
            *dockerfile = "Failfile".into();
            source
        }
    }

    struct RepositoryFixture {
        directory: tempfile::TempDir,
    }

    impl RepositoryFixture {
        fn new() -> Self {
            let fixture = Self {
                directory: tempfile::tempdir().unwrap(),
            };
            fixture.git(&["init", "--initial-branch=main"]);
            fixture
        }
        fn git(&self, args: &[&str]) -> String {
            let output = std::process::Command::new("git")
                .current_dir(self.directory.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        }
        fn manifest(&self, path: &str) -> ApplicationManifest {
            let mut application = manifest();
            application.spec.manifest = Some(RepositoryManifest {
                repository: GitRepository {
                    url: self.directory.path().display().to_string(),
                    branch: "main".into(),
                    commit: None,
                },
                path: path.into(),
            });
            application
        }
        fn write(&self, path: &str, manifest: &ApplicationManifest) {
            std::fs::write(
                self.directory.path().join(path),
                serde_json::to_vec(manifest).unwrap(),
            )
            .unwrap();
        }
        fn commit(&self) -> String {
            self.git(&["add", "--all"]);
            self.git(&[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "fixture",
            ]);
            self.git(&["rev-parse", "HEAD"])
        }
        async fn deploy(harness: &ControllerHarness, id: &ApplicationId) -> Operation {
            let MutationResponse::Operation(accepted) = harness
                .applications()
                .accept(Mutation::Deploy { id: id.clone() }, None, true, None)
                .await
                .unwrap()
            else {
                panic!("operation expected")
            };
            harness
                .controller
                .scan(&CancellationToken::new())
                .await
                .unwrap();
            harness
                .store
                .operation(&accepted.operation_id)
                .await
                .unwrap()
        }
    }

    #[tokio::test]
    async fn deploy_updates_only_selected_manifest_then_follows_its_new_path_and_disconnects() {
        let repository = RepositoryFixture::new();
        let harness = ControllerHarness::new().await;
        let initial = repository.manifest("app.json");
        let first = harness
            .applications()
            .apply(initial.clone().validate().unwrap(), Some(0))
            .await
            .unwrap();
        harness.finish(&first).await;
        let before = harness.pulls().await;
        let mut next = repository.manifest("next.json");
        next.spec.services[0].replicas = 2;
        repository.write("app.json", &next);
        repository.write("next.json", &next);
        std::fs::write(
            repository.directory.path().join("other.json"),
            "not a manifest",
        )
        .unwrap();
        repository.commit();
        let deployed = RepositoryFixture::deploy(&harness, &first.application_id).await;
        assert_eq!(deployed.state, OperationState::Succeeded);
        assert_eq!(deployed.generation, 2);
        assert!(
            harness.pulls().await > before,
            "unchanged image references must be refreshed"
        );
        let current = harness.store.get(&first.application_id).await.unwrap();
        assert_eq!(
            current.application.spec().manifest.as_ref().unwrap().path,
            "next.json"
        );
        assert_eq!(current.resolved.unwrap().services[0].replicas, 2);
        assert_eq!(harness.store.list(None, 50).await.unwrap().items.len(), 1);
        next.spec.manifest = None;
        next.spec.services.clear();
        repository.write("next.json", &next);
        std::fs::remove_file(repository.directory.path().join("app.json")).unwrap();
        repository.commit();
        assert_eq!(
            RepositoryFixture::deploy(&harness, &first.application_id)
                .await
                .state,
            OperationState::Succeeded
        );
        assert!(
            harness
                .store
                .get(&first.application_id)
                .await
                .unwrap()
                .application
                .spec()
                .manifest
                .is_none()
        );
        std::fs::remove_file(repository.directory.path().join("next.json")).unwrap();
        repository.commit();
        assert_eq!(
            RepositoryFixture::deploy(&harness, &first.application_id)
                .await
                .state,
            OperationState::Succeeded
        );
    }

    #[tokio::test]
    async fn missing_invalid_and_failed_build_manifests_preserve_accepted_configuration() {
        let repository = RepositoryFixture::new();
        repository.commit();
        let harness = ControllerHarness::new().await;
        let initial = repository.manifest("app.json");
        let first = harness
            .applications()
            .apply(initial.clone().validate().unwrap(), Some(0))
            .await
            .unwrap();
        harness.finish(&first).await;
        let before = harness.store.get(&first.application_id).await.unwrap();
        let missing = RepositoryFixture::deploy(&harness, &first.application_id).await;
        assert_eq!(missing.error_code.as_deref(), Some("manifest_not_found"));
        let mut invalid = initial.clone();
        invalid.metadata.name = "different-application".into();
        repository.write("app.json", &invalid);
        repository.commit();
        let invalid = RepositoryFixture::deploy(&harness, &first.application_id).await;
        assert_eq!(invalid.error_code.as_deref(), Some("manifest_invalid"));
        let build = git_fixture::GitBuildFixture::new();
        let mut failing = initial.clone();
        failing.spec.manifest = None;
        failing.spec.services[0].source = build.failing_source();
        repository.write("app.json", &failing);
        repository.commit();
        let failed = RepositoryFixture::deploy(&harness, &first.application_id).await;
        assert_eq!(failed.error_code.as_deref(), Some("git_build_failed"));
        let after = harness.store.get(&first.application_id).await.unwrap();
        assert_eq!(before.application, after.application);
        assert_eq!(before.generation, after.generation);
        assert_eq!(before.resolved, after.resolved);
        assert_eq!(harness.store.list(None, 50).await.unwrap().items.len(), 1);
        let mut manual = initial;
        manual.spec.services[0].replicas = 4;
        assert!(matches!(
            harness
                .applications()
                .apply(manual.validate().unwrap(), Some(1))
                .await,
            Err(ApplicationError::Store(StoreError::RepositoryManaged))
        ));
    }

    #[tokio::test]
    async fn retry_after_reopening_store_uses_fetched_manifest_and_new_deploy_fetches_again() {
        let repository = RepositoryFixture::new();
        let harness = ControllerHarness::new().await;
        let initial = repository.manifest("app.json");
        let first = harness
            .applications()
            .apply(initial.clone().validate().unwrap(), Some(0))
            .await
            .unwrap();
        harness.finish(&first).await;
        let build = git_fixture::GitBuildFixture::new();
        let mut candidate = initial.clone();
        candidate.spec.services[0].source = build.failing_source();
        repository.write("app.json", &candidate);
        repository.commit();
        let failed = RepositoryFixture::deploy(&harness, &first.application_id).await;
        assert_eq!(failed.error_code.as_deref(), Some("git_build_failed"));
        repository.write("app.json", &initial);
        repository.commit();
        let reopened = Arc::new(Store::open(&harness.database_path).await.unwrap());
        let controller = Controller::new(Arc::clone(&harness.docker), Arc::clone(&reopened));
        let applications = TestApplications::new(
            Arc::clone(&reopened),
            controller.runtime(Arc::new(tokio::sync::Notify::new())),
        );
        let retry = applications
            .reconcile(&first.application_id, None)
            .await
            .unwrap();
        assert_eq!(retry.id, failed.id);
        controller.scan(&CancellationToken::new()).await.unwrap();
        assert_eq!(
            reopened.operation(&retry.id).await.unwrap().state,
            OperationState::Failed
        );
        assert_eq!(
            RepositoryFixture::deploy(&harness, &first.application_id)
                .await
                .state,
            OperationState::Succeeded
        );
    }

    #[tokio::test]
    async fn bootstrap_can_pin_manifest_revision_independently_from_git_build_revision() {
        let repository = RepositoryFixture::new();
        let harness = ControllerHarness::new().await;
        let build_repository = RepositoryFixture::new();
        std::fs::write(
            build_repository.directory.path().join("Dockerfile"),
            "FROM alpine:3.20\n",
        )
        .unwrap();
        let build_commit = build_repository.commit();
        let mut fetched = repository.manifest("app.json");
        fetched.spec.services[0].source = piqueld_core::Source::Git {
            repository: GitRepository {
                url: build_repository.directory.path().display().to_string(),
                branch: "main".into(),
                commit: Some(build_commit.clone()),
            },
            build: piqueld_core::manifest::Build::Docker {
                dockerfile: "Dockerfile".into(),
                context: ".".into(),
            },
        };
        repository.write("app.json", &fetched);
        let pinned = repository.commit();
        let mut bootstrap = repository.manifest("app.json");
        bootstrap.spec.services.clear();
        bootstrap.spec.manifest.as_mut().unwrap().repository.commit = Some(pinned);
        fetched.metadata.name = "wrong-later-name".into();
        repository.write("app.json", &fetched);
        repository.commit();
        let first = harness
            .applications()
            .apply(bootstrap.validate().unwrap(), Some(0))
            .await
            .unwrap();
        harness.finish(&first).await;
        let deployment = RepositoryFixture::deploy(&harness, &first.application_id).await;
        assert_eq!(deployment.state, OperationState::Succeeded);
        let resolved = harness
            .store
            .get(&first.application_id)
            .await
            .unwrap()
            .resolved
            .unwrap();
        assert!(
            matches!(&resolved.services[0].source, ResolvedSource::Git { commit, .. } if commit == &build_commit)
        );
    }
}

#[derive(Clone, Default)]
struct TraceCapture(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for TraceCapture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
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
        harness.application = manifest
            .validate()
            .unwrap()
            .normalize(harness.application.id().clone());
        harness.resolved = compile_application(
            &harness.application,
            InstanceId::parse(harness.store.instance_id()).unwrap(),
            &harness.resolutions,
        )
        .unwrap();
        let operation = harness.create().await;
        harness
            .controller
            .scan(&CancellationToken::new())
            .await
            .unwrap();
        (harness.application.id().to_string(), operation.id)
    };
    let text = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
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
    assert_eq!(completed["span"]["application_id"], application_id);
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

impl ControllerHarness {
    async fn assert_git_build_history(&self, id: &ApplicationId) {
        let builds = self.store.builds(Some(id), None, 50).await.unwrap();
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

#[tokio::test]
async fn secret_rotation_requires_deploy_and_service_references_follow_pinned_versions() {
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let first = applications
        .apply(manifest().validate().unwrap(), Some(0))
        .await
        .unwrap();
    harness.finish(&first).await;
    harness
        .store
        .put_secret(&first.application_id, "token", 0, b"version-one".to_vec())
        .await
        .unwrap();
    let mut input = manifest();
    input.spec.services[0]
        .secrets
        .push(piqueld_core::manifest::SecretMount {
            name: "token".into(),
            target: "/run/secrets/token".into(),
        });
    let deployment = applications
        .apply(input.validate().unwrap(), Some(1))
        .await
        .unwrap();
    harness.finish(&deployment).await;
    let target = harness.target(&first.application_id).await;
    let old = target.secret_names["token"].clone();
    assert_eq!(
        harness.docker.secret_values.lock().await[&old],
        b"version-one"
    );
    harness
        .store
        .put_secret(&first.application_id, "token", 1, b"version-two".to_vec())
        .await
        .unwrap();
    harness
        .controller
        .scan(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        harness.target(&first.application_id).await,
        target,
        "rotation alone must not update services"
    );
    let next = applications
        .deploy(&first.application_id, None)
        .await
        .unwrap();
    harness.finish(&next).await;
    let target = harness.target(&first.application_id).await;
    let new = &target.secret_names["token"];
    assert_ne!(&old, new);
    assert_eq!(
        harness.docker.secret_values.lock().await[new],
        b"version-two"
    );
    assert_eq!(
        harness
            .docker
            .observe(&first.application_id)
            .await
            .unwrap()
            .services[0]
            .secrets,
        target.services[0].secrets
    );

    // Historical completed deployments must not block deletion forever.
    let clean = applications
        .apply(manifest().validate().unwrap(), Some(2))
        .await
        .unwrap();
    harness.finish(&clean).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let service = piqueld::api::ApplicationService::new(
        Arc::clone(&harness.store),
        harness
            .controller
            .runtime(Arc::new(tokio::sync::Notify::new())),
    );
    let server = tokio::spawn({
        use std::future::IntoFuture;
        axum::serve(listener, piqueld::api::http::router(service)).into_future()
    });
    let client = piqueld_client::Client::tcp(&format!("http://{address}")).unwrap();
    client
        .delete_secret(first.application_id.as_str(), "token", 2)
        .await
        .unwrap();
    assert!(harness.docker.secret_values.lock().await.is_empty());
    assert!(
        harness
            .store
            .secrets(&first.application_id)
            .await
            .unwrap()
            .is_empty()
    );
    server.abort();
}

#[tokio::test]
async fn secret_key_recovery_leaves_running_services_and_blocks_unavailable_rollouts() {
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let initial = applications
        .apply(manifest().validate().unwrap(), Some(0))
        .await
        .unwrap();
    harness.finish(&initial).await;
    let id = &initial.application_id;
    harness
        .store
        .put_secret(id, "token", 0, b"original".to_vec())
        .await
        .unwrap();
    let mut input = manifest();
    input.spec.services[0]
        .secrets
        .push(piqueld_core::manifest::SecretMount {
            name: "token".into(),
            target: "/run/secrets/token".into(),
        });
    let deployed = applications
        .apply(input.validate().unwrap(), Some(1))
        .await
        .unwrap();
    harness.finish(&deployed).await;
    let target = harness.target(id).await;
    let original = harness.docker.observe(id).await.unwrap();
    let values = harness.docker.secret_values.lock().await.clone();

    harness.store.replace_secret_key(false, None).await.unwrap();
    harness
        .controller
        .scan(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(harness.target(id).await, target);
    assert_eq!(harness.docker.observe(id).await.unwrap(), original);

    harness.store.replace_secret_key(true, None).await.unwrap();
    harness
        .controller
        .scan(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(harness.target(id).await, target);
    assert_eq!(harness.docker.observe(id).await.unwrap(), original);
    assert_eq!(*harness.docker.secret_values.lock().await, values);

    // Neither active-target repair nor a new deployment may change runtime
    // resources when the target still depends on discarded values.
    harness.docker.observed.lock().await.networks.clear();
    let before_failed = harness.docker.observe(id).await.unwrap();
    let failed = applications.deploy(id, None).await.unwrap();
    harness
        .controller
        .scan(&CancellationToken::new())
        .await
        .unwrap();
    let result = harness.store.operation(&failed.id).await.unwrap();
    assert_eq!(result.error_code.as_deref(), Some("secret_unavailable"));
    assert!(result.error_message.as_deref().unwrap().contains("token"));
    assert_eq!(harness.docker.observe(id).await.unwrap(), before_failed);

    harness
        .store
        .put_secret(id, "token", 1, b"replacement".to_vec())
        .await
        .unwrap();
    let replacement = applications.deploy(id, None).await.unwrap();
    harness.finish(&replacement).await;
    let new_target = harness.target(id).await;
    assert_ne!(new_target.secret_names, target.secret_names);
    assert_eq!(
        harness.docker.secret_values.lock().await[&new_target.secret_names["token"]],
        b"replacement"
    );
    assert_eq!(
        harness.docker.secret_values.lock().await[&target.secret_names["token"]],
        b"original"
    );
}
