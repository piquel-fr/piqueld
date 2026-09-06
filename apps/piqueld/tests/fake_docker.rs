//! Reconciliation coverage using the real Docker seam and an in-memory backend.

use async_trait::async_trait;
use piqueld::docker::{DockerApi, DockerError, ImageSource, SwarmState, resolve_image_digest};
use piqueld::operations::OperationScheduler;
use piqueld::reconcile::ReconcileHandler;
use piqueld::store::{SqliteStore, WorkState};
use piqueld_core::Sha256Digest;
use piqueld_core::planner::PlanRequest;
use piqueld_core::resource::{
    APPLICATION_LABEL, Convergence, DesiredService, INSTANCE_LABEL, MANAGED_LABEL,
    ObservedApplication, ObservedNetwork, ObservedService, ObservedTask, ObservedVolume,
    ResolutionSet, ResolvedApplication, ResolvedSource, SERVICE_LABEL, SPEC_HASH_LABEL, TaskState,
    compile_application, image_repository,
};
use piqueld_core::{ApplicationId, InstanceId, Plan, PlanAction, parse_toml};
use sqlx::{Connection, SqliteConnection};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
struct FakeDocker {
    observed: Arc<Mutex<ObservedApplication>>,
    registry: Arc<Mutex<RegistryState>>,
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

impl FakeDocker {
    fn with_observed(observed: ObservedApplication) -> Self {
        Self {
            observed: Arc::new(Mutex::new(observed)),
            registry: Arc::new(Mutex::new(RegistryState::default())),
        }
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
        name: desired.name.clone(),
        image: desired.image.clone(),
        replicas: desired.replicas,
        environment: desired.environment.clone(),
        command: desired.command.clone(),
        arguments: desired.arguments.clone(),
        mounts: desired.mounts.clone(),
        healthcheck: desired.healthcheck.clone(),
        healthcheck_configured: desired.healthcheck.is_some(),
        resources: desired.resources.clone(),
        networks: desired.networks.clone(),
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
    async fn ensure_swarm(&self, _auto_initialize: bool) -> Result<SwarmState, DockerError> {
        Ok(SwarmState::Ready)
    }

    async fn resolve_image(&self, reference: &str) -> Result<String, DockerError> {
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
        _application: &ApplicationId,
    ) -> Result<ObservedApplication, DockerError> {
        Ok(self.observed.lock().await.clone())
    }

    async fn ensure_network(
        &self,
        desired: &piqueld_core::resource::DesiredNetwork,
    ) -> Result<(), DockerError> {
        let mut observed = self.observed.lock().await;
        if let Some(existing) = observed
            .networks
            .iter()
            .find(|network| network.name == desired.name)
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
            name: desired.name.clone(),
            runtime_configuration_matches: true,
            labels: desired.labels.clone(),
        });
        Ok(())
    }

    async fn ensure_volume(
        &self,
        desired: &piqueld_core::resource::DesiredVolume,
    ) -> Result<(), DockerError> {
        let mut observed = self.observed.lock().await;
        if let Some(existing) = observed
            .volumes
            .iter()
            .find(|volume| volume.name == desired.name)
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
            name: desired.name.clone(),
            runtime_configuration_matches: true,
            labels: desired.labels.clone(),
        });
        Ok(())
    }

    async fn ensure_service(
        &self,
        desired: &piqueld_core::resource::DesiredService,
    ) -> Result<(), DockerError> {
        let mut observed = self.observed.lock().await;
        if let Some(existing) = observed
            .services
            .iter()
            .find(|service| service.name == desired.name)
            && !Self::ownership_matches(&existing.labels, &desired.labels)
        {
            return Err(DockerError::OwnershipConflict);
        }
        observed
            .services
            .retain(|service| service.name != desired.name);
        observed.services.push(observed_service(desired));
        Ok(())
    }

    async fn remove_service(
        &self,
        name: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
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
    Arc<SqliteStore>,
    piqueld_core::NormalizedApplication,
    ResolvedApplication,
) {
    let store = Arc::new(
        SqliteStore::open(directory.path().join("control-plane.db"))
            .await
            .expect("fresh database opens"),
    );
    let application = application();
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

struct SchedulerHarness {
    _directory: tempfile::TempDir,
    database_path: PathBuf,
    store: Arc<SqliteStore>,
    application: piqueld_core::NormalizedApplication,
    resolutions: ResolutionSet,
    resolved: ResolvedApplication,
    docker: Arc<FakeDocker>,
    scheduler: OperationScheduler<ReconcileHandler<FakeDocker>>,
}

impl SchedulerHarness {
    async fn new() -> Self {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database_path = directory.path().join("control-plane.db");
        let store = Arc::new(
            SqliteStore::open(&database_path)
                .await
                .expect("fresh database opens"),
        );
        let application = application();
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
        let resolved = compile_application(
            &application,
            InstanceId::parse(store.instance_id()).expect("store instance ID is valid"),
            &resolutions,
        )
        .expect("fixture resolves");
        let docker = Arc::new(FakeDocker::default());
        let handler = Arc::new(ReconcileHandler::new(
            Arc::clone(&docker),
            Arc::clone(&store),
        ));
        let scheduler = OperationScheduler::new(store.clone(), handler, 1);
        Self {
            _directory: directory,
            database_path,
            store,
            application,
            resolutions,
            resolved,
            docker,
            scheduler,
        }
    }

    fn steps(plan: &piqueld_core::Plan) -> Vec<String> {
        plan.actions
            .iter()
            .map(PlanAction::operation_step)
            .collect()
    }

    fn reconcile_plan(
        desired: ResolvedApplication,
        observed: &ObservedApplication,
    ) -> piqueld_core::Plan {
        Plan::from_request(&PlanRequest::Reconcile { desired }, observed)
    }

    async fn create(&self) -> piqueld::store::MutationResult {
        let initial_plan =
            Self::reconcile_plan(self.resolved.clone(), &ObservedApplication::default());
        let steps = Self::steps(&initial_plan);
        self.store
            .create(&self.application, &self.resolved, &steps)
            .await
            .expect("application is created")
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
        sqlx::query(
            "UPDATE operation_steps SET state='running',started_at_ms=created_at_ms,updated_at_ms=created_at_ms WHERE operation_id=?1",
        )
        .bind(operation_id)
        .execute(&mut connection)
        .await
        .expect("operation steps can be interrupted");
    }

    async fn assert_recovered(&self, operation_id: &str) {
        let status = self
            .store
            .status(&self.application.id)
            .await
            .expect("status is readable");
        assert_eq!(status.state, piqueld::store::ApplicationState::Ready);
        let (operation, steps) = self
            .store
            .operation_with_steps(operation_id)
            .await
            .expect("operation journal is readable");
        assert_eq!(operation.state, piqueld::store::WorkState::Succeeded);
        assert!(steps.iter().all(|step| {
            matches!(
                step.state,
                piqueld::store::StepState::Succeeded | piqueld::store::StepState::Skipped
            )
        }));
        let observed = self
            .docker
            .observe(&self.application.id)
            .await
            .expect("fake observation");
        assert_eq!(observed.volumes.len(), 1);
        assert_eq!(observed.services.len(), 1);
    }

    async fn replace(&self) -> (piqueld::store::MutationResult, ResolvedApplication) {
        let mut replacement = self.application.clone();
        replacement.spec.services[0].replicas = 2;
        let replacement = replacement.normalize();
        let replacement_resolved = compile_application(
            &replacement,
            InstanceId::parse(self.store.instance_id()).expect("store instance ID is valid"),
            &self.resolutions,
        )
        .expect("replacement resolves");
        let observed = self
            .docker
            .observe(&self.application.id)
            .await
            .expect("observation");
        let replacement_plan = Self::reconcile_plan(replacement_resolved.clone(), &observed);
        let steps = Self::steps(&replacement_plan);
        let replaced = self
            .store
            .replace(&replacement, &replacement_resolved, 1, &steps)
            .await
            .expect("application replacement is durable");
        (replaced, replacement_resolved)
    }

    async fn repair_drift(&self, desired: &ResolvedApplication) {
        {
            let mut observed = self.docker.observed.lock().await;
            observed.services[0].replicas = 1;
            observed.services[0].convergence = Convergence::Degraded;
        }
        let observed = self
            .docker
            .observe(&self.application.id)
            .await
            .expect("drift observation");
        let drift_plan = Self::reconcile_plan(desired.clone(), &observed);
        let steps = Self::steps(&drift_plan);
        let drift = self
            .store
            .request_reconcile(&self.application.id, 2, &steps)
            .await
            .expect("drift repair is durable");
        assert_eq!(drift.generation, 2);
        self.scheduler
            .recover_and_run(CancellationToken::new())
            .await
            .expect("drift repair converges");
        assert_eq!(
            self.docker
                .observe(&self.application.id)
                .await
                .unwrap()
                .services[0]
                .replicas,
            2
        );
    }

    async fn delete(&self, desired: &ResolvedApplication) -> piqueld::store::MutationResult {
        let observed = self
            .docker
            .observe(&self.application.id)
            .await
            .expect("delete observation");
        let deletion_plan = Plan::from_request(
            &PlanRequest::Delete {
                application_id: self.application.id.clone(),
                instance_id: desired.instance_id.clone(),
            },
            &observed,
        );
        let steps = Self::steps(&deletion_plan);
        self.store
            .request_delete(&self.application.id, 2, &steps)
            .await
            .expect("delete is durable")
    }

    async fn assert_deleted(&self) {
        assert!(matches!(
            self.store.get(&self.application.id).await,
            Err(piqueld::store::StoreError::NotFound)
        ));
        let observed = self
            .docker
            .observe(&self.application.id)
            .await
            .expect("final observation");
        assert!(observed.services.is_empty());
        assert!(observed.networks.is_empty());
        assert_eq!(observed.volumes.len(), 1);
    }
}

#[tokio::test]
async fn scheduler_converges_a_prebuilt_application_through_the_docker_seam() {
    let harness = SchedulerHarness::new().await;
    let created = harness.create().await;
    harness.interrupt(&created.operation_id).await;
    harness
        .scheduler
        .recover_and_run(CancellationToken::new())
        .await
        .expect("scheduler converges the operation");
    harness.assert_recovered(&created.operation_id).await;

    let (replaced, replacement) = harness.replace().await;
    harness
        .scheduler
        .recover_and_run(CancellationToken::new())
        .await
        .expect("replacement converges");
    assert_eq!(replaced.generation, 2);
    assert_eq!(
        harness
            .docker
            .observe(&harness.application.id)
            .await
            .unwrap()
            .services[0]
            .replicas,
        2
    );

    harness
        .scheduler
        .recover_and_run(CancellationToken::new())
        .await
        .expect("matching state is idempotent");
    assert!(
        harness
            .store
            .active_reconcile(&harness.application.id, 2)
            .await
            .unwrap()
            .is_none()
    );

    harness.repair_drift(&replacement).await;
    let deleted = harness.delete(&replacement).await;
    assert_eq!(deleted.generation, 3);
    harness
        .scheduler
        .recover_and_run(CancellationToken::new())
        .await
        .expect("delete converges");
    harness.assert_deleted().await;
}

#[tokio::test]
async fn scheduler_journals_and_executes_actions_introduced_by_fresh_planning() {
    let harness = SchedulerHarness::new().await;
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
        .observe(&harness.application.id)
        .await
        .expect("matching observation");
    let plan = SchedulerHarness::reconcile_plan(harness.resolved.clone(), &observed);
    assert!(plan.actions.is_empty());
    let created = harness
        .store
        .create(&harness.application, &harness.resolved, &[])
        .await
        .expect("matching application is journaled");

    harness.docker.observed.lock().await.networks.clear();
    harness
        .scheduler
        .recover_and_run(CancellationToken::new())
        .await
        .expect("fresh action converges");

    let status = harness.store.status(&harness.application.id).await.unwrap();
    assert_eq!(status.state, piqueld::store::ApplicationState::Ready);
    let (_, steps) = harness
        .store
        .operation_with_steps(&created.operation_id)
        .await
        .expect("fresh steps are journaled");
    assert!(
        steps
            .iter()
            .any(|step| step.action.starts_with("ENSURE NETWORK "))
    );
    assert_eq!(harness.docker.observed.lock().await.networks.len(), 1);
}

#[tokio::test]
async fn superseded_operations_do_not_plan_stale_runtime_state() {
    let harness = SchedulerHarness::new().await;
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
        .observe(&harness.application.id)
        .await
        .expect("matching observation");
    let plan = SchedulerHarness::reconcile_plan(harness.resolved.clone(), &observed);
    assert!(plan.actions.is_empty());
    let stale = harness
        .store
        .create(&harness.application, &harness.resolved, &[])
        .await
        .expect("matching application is journaled");

    harness.docker.observed.lock().await.networks.clear();
    let (replacement, _) = harness.replace().await;
    harness
        .scheduler
        .recover_and_run(CancellationToken::new())
        .await
        .expect("the current generation converges");

    let (stale_operation, stale_steps) = harness
        .store
        .operation_with_steps(&stale.operation_id)
        .await
        .expect("superseded operation is readable");
    assert_eq!(stale_operation.state, WorkState::Cancelled);
    assert!(stale_steps.is_empty());
    let (replacement_operation, _) = harness
        .store
        .operation_with_steps(&replacement.operation_id)
        .await
        .expect("replacement operation is readable");
    assert_eq!(replacement_operation.state, WorkState::Succeeded);
}

#[tokio::test]
async fn scheduler_refuses_a_foreign_same_name_service() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let (store, application, resolved) = fixture_store(&directory).await;
    let mut foreign_service_labels = foreign_labels(&application.id);
    foreign_service_labels.insert(SERVICE_LABEL.into(), "web".into());
    let foreign = ObservedService {
        labels: foreign_service_labels,
        ..observed_service(&resolved.services[0])
    };
    let initial_plan = Plan::from_request(
        &PlanRequest::Reconcile {
            desired: resolved.clone(),
        },
        &ObservedApplication::default(),
    );
    let created = store
        .create(
            &application,
            &resolved,
            &initial_plan
                .actions
                .iter()
                .map(PlanAction::operation_step)
                .collect::<Vec<_>>(),
        )
        .await
        .expect("application is created");
    let docker = Arc::new(FakeDocker::with_observed(ObservedApplication {
        services: vec![foreign],
        ..ObservedApplication::default()
    }));
    let handler = Arc::new(ReconcileHandler::new(
        Arc::clone(&docker),
        Arc::clone(&store),
    ));
    let scheduler = OperationScheduler::new(store.clone(), handler, 1);
    scheduler
        .recover_and_run(CancellationToken::new())
        .await
        .expect("ownership conflict is journaled");
    let status = store
        .status(&application.id)
        .await
        .expect("status is readable");
    assert_eq!(status.state, piqueld::store::ApplicationState::Degraded);
    let (operation, steps) = store
        .operation_with_steps(&created.operation_id)
        .await
        .expect("failed operation is readable");
    assert_eq!(operation.state, piqueld::store::WorkState::Failed);
    assert!(steps.iter().any(|step| {
        step.state == piqueld::store::StepState::Cancelled && step.error_code.is_none()
    }));
    assert_eq!(
        docker
            .observe(&application.id)
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
    store: &Arc<SqliteStore>,
    application: &piqueld_core::NormalizedApplication,
    resolved: &ResolvedApplication,
) -> piqueld::store::MutationResult {
    let initial_plan = Plan::from_request(
        &PlanRequest::Reconcile {
            desired: resolved.clone(),
        },
        &ObservedApplication::default(),
    );
    let created = store
        .create(
            application,
            resolved,
            &initial_plan
                .actions
                .iter()
                .map(PlanAction::operation_step)
                .collect::<Vec<_>>(),
        )
        .await
        .expect("application is created");
    let handler = Arc::new(ReconcileHandler::new(Arc::clone(docker), Arc::clone(store)));
    let scheduler = OperationScheduler::new(Arc::clone(store), handler, 1);
    scheduler
        .recover_and_run(CancellationToken::new())
        .await
        .expect("ownership conflict is journaled");
    let status = store
        .status(&application.id)
        .await
        .expect("status is readable");
    assert_eq!(status.state, piqueld::store::ApplicationState::Degraded);
    let (operation, steps) = store
        .operation_with_steps(&created.operation_id)
        .await
        .expect("failed operation is readable");
    assert_eq!(operation.state, piqueld::store::WorkState::Failed);
    assert!(steps.iter().any(|step| {
        step.state == piqueld::store::StepState::Cancelled && step.error_code.is_none()
    }));
    created
}

#[tokio::test]
async fn scheduler_refuses_a_foreign_same_name_network() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let (store, application, resolved) = fixture_store(&directory).await;
    let foreign = ObservedNetwork {
        name: resolved.networks[0].name.clone(),
        runtime_configuration_matches: true,
        labels: foreign_labels(&application.id),
    };
    let docker = Arc::new(FakeDocker::with_observed(ObservedApplication {
        networks: vec![foreign],
        ..ObservedApplication::default()
    }));
    let created =
        assert_foreign_fixture_refuses_reconciliation(&docker, &store, &application, &resolved)
            .await;
    assert_eq!(created.generation, 1);
    // The foreign network must survive untouched.
    let observed = docker.observe(&application.id).await.unwrap();
    assert_eq!(observed.networks.len(), 1);
    assert_eq!(
        observed.networks[0].labels.get(INSTANCE_LABEL),
        Some(&"other-instance".to_string())
    );
}

#[tokio::test]
async fn scheduler_refuses_a_foreign_same_name_volume() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let (store, application, resolved) = fixture_store(&directory).await;
    let foreign = ObservedVolume {
        name: resolved.volumes[0].name.clone(),
        runtime_configuration_matches: true,
        labels: foreign_labels(&application.id),
    };
    let docker = Arc::new(FakeDocker::with_observed(ObservedApplication {
        volumes: vec![foreign],
        ..ObservedApplication::default()
    }));
    let created =
        assert_foreign_fixture_refuses_reconciliation(&docker, &store, &application, &resolved)
            .await;
    assert_eq!(created.generation, 1);
    // The foreign volume must survive untouched.
    let observed = docker.observe(&application.id).await.unwrap();
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
