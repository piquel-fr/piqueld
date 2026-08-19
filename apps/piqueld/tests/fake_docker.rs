//! Reconciliation coverage using the real Docker seam and an in-memory backend.

use async_trait::async_trait;
use piqueld::docker::{DockerApi, DockerError, SwarmState};
use piqueld::operations::OperationScheduler;
use piqueld::reconcile::ReconcileHandler;
use piqueld::store::SqliteStore;
use piqueld_core::planner::PlanRequest;
use piqueld_core::resource::{
    Convergence, DesiredService, ObservedApplication, ObservedNetwork, ObservedService,
    ObservedTask, ObservedVolume, ResolutionSet, ResolvedApplication, ResolvedSource, TaskState,
    compile_application,
};
use piqueld_core::{ApplicationId, InstanceId, PlanAction, parse_toml, plan};
use sqlx::{Connection, SqliteConnection};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
struct FakeDocker {
    observed: Arc<Mutex<ObservedApplication>>,
}

impl FakeDocker {
    fn with_observed(observed: ObservedApplication) -> Self {
        Self {
            observed: Arc::new(Mutex::new(observed)),
        }
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
        Ok(format!("{reference}@sha256:{}", "a".repeat(64)))
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
        if !observed
            .networks
            .iter()
            .any(|network| network.name == desired.name)
        {
            observed.networks.push(ObservedNetwork {
                name: desired.name.clone(),
                runtime_configuration_matches: true,
                labels: desired.labels.clone(),
            });
        }
        Ok(())
    }

    async fn ensure_volume(
        &self,
        desired: &piqueld_core::resource::DesiredVolume,
    ) -> Result<(), DockerError> {
        let mut observed = self.observed.lock().await;
        if !observed
            .volumes
            .iter()
            .any(|volume| volume.name == desired.name)
        {
            observed.volumes.push(ObservedVolume {
                name: desired.name.clone(),
                runtime_configuration_matches: true,
                labels: desired.labels.clone(),
            });
        }
        Ok(())
    }

    async fn ensure_service(
        &self,
        desired: &piqueld_core::resource::DesiredService,
    ) -> Result<(), DockerError> {
        let mut observed = self.observed.lock().await;
        observed
            .services
            .retain(|service| service.name != desired.name);
        observed.services.push(observed_service(desired));
        Ok(())
    }

    async fn remove_service(
        &self,
        name: &str,
        _ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        self.observed
            .lock()
            .await
            .services
            .retain(|service| service.name != name);
        Ok(())
    }

    async fn remove_network(
        &self,
        name: &str,
        _ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        self.observed
            .lock()
            .await
            .networks
            .retain(|network| network.name != name);
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
        plan(&PlanRequest::Reconcile { desired }, observed)
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
        let deletion_plan = plan(
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
async fn scheduler_refuses_a_foreign_same_name_service() {
    let directory = tempfile::tempdir().expect("temporary directory");
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
    let foreign = ObservedService {
        labels: BTreeMap::from([
            ("io.piqueld.managed".into(), "true".into()),
            ("io.piqueld.instance".into(), "other-instance".into()),
            ("io.piqueld.application".into(), application.id.to_string()),
            ("io.piqueld.service".into(), "web".into()),
            (
                "io.piqueld.spec-hash".into(),
                format!("sha256:{}", "b".repeat(64)),
            ),
        ]),
        ..observed_service(&resolved.services[0])
    };
    let initial_plan = plan(
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
