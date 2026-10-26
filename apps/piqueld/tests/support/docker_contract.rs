//! Scenarios every [`DockerApi`] adapter passes, so `FakeDocker`, which the
//! controller tests run against, behaves like the real Engine where the
//! controller relies on it. `fake_docker.rs` runs them against the fake and
//! `docker_integration.rs` against an isolated Engine: each declares one test
//! per scenario through [`scenarios`].

use crate::poll::eventually;
use piqueld::docker::{DockerApi, DockerError, JobOutput, JobRuns, JobStatus};
use piqueld::operations::OperationError;
use piqueld_core::api::LogStream;
use piqueld_core::manifest::{JobVolumeAccess, ValidatedHealthCheck};
use piqueld_core::observability::DiagnosticCode;
use piqueld_core::resource::{
    Convergence, DesiredNetwork, DesiredService, DesiredVolume, JOB_LABEL, ObservedApplication,
    ObservedService, Ownership, ResolvedSource, SERVICE_LABEL, TaskState,
};
use piqueld_core::{EnvironmentId, InstanceId, ResourceKind, docker_resource_name};
use std::{collections::BTreeMap, time::Duration};

/// The image every service and job runs.
pub const IMAGE: &str = "alpine:3.20";
/// A tag of [`IMAGE`]'s repository that doesn't exist.
pub const MISSING_IMAGE: &str = "alpine:piqueld-missing";
/// Another repository serving [`IMAGE`]'s images.
pub const MIRROR: &str = "mirror.gcr.io/library/alpine";

/// How long a scenario waits for the Engine to apply a change; the fake
/// applies each at once.
const CONVERGENCE: Duration = Duration::from_mins(2);
/// How often a scenario checks whether a change was applied.
const POLL: Duration = Duration::from_millis(250);

/// Calls `$test!(scenario)` for every scenario of [`Contract`], so each test
/// binary declares the same tests.
macro_rules! scenarios {
    ($test:ident) => {
        $test!(ensured_resources_are_observed_and_ensuring_again_changes_nothing);
        $test!(service_updates_are_observed);
        $test!(detaching_a_network_changes_only_the_service_networks);
        $test!(removing_services_and_networks_is_idempotent);
        $test!(secrets_are_only_changed_by_their_owner);
        $test!(missing_images_are_rejected);
        $test!(pinned_references_resolve_to_their_digest);
        $test!(an_image_in_two_repositories_lists_both_digests);
        $test!(job_runs_report_their_status_and_output_per_operation);
        $test!(observations_report_only_their_environment);
    };
}
pub(crate) use scenarios;

/// One environment of the `integration-instance` installation, and the
/// resources piqueld desires for it.
pub struct Environment {
    /// Its ID, unique to this run.
    pub app: EnvironmentId,
    /// Ownership labels of its networks, volumes, and jobs.
    pub labels: BTreeMap<String, String>,
    /// [`IMAGE`], resolved to a repository digest.
    pub image: String,
}

impl Environment {
    /// A new environment whose services run [`IMAGE`] as `docker` resolves it.
    pub async fn new(docker: &impl DockerApi) -> Self {
        let suffix = uuid::Uuid::now_v7().simple().to_string();
        let app = EnvironmentId::parse(format!("app-{}", &suffix[..16])).unwrap();
        let mut labels =
            Ownership::scope(&InstanceId::parse("integration-instance").unwrap(), &app);
        labels.insert(
            piqueld_core::resource::SPEC_HASH_LABEL.into(),
            format!("sha256:{}", "a".repeat(64)),
        );
        Self {
            image: docker.resolve_image(IMAGE).await.unwrap(),
            app,
            labels,
        }
    }

    /// The installation that owns its resources.
    pub fn instance(&self) -> InstanceId {
        InstanceId::parse(&self.labels[piqueld_core::resource::INSTANCE_LABEL]).unwrap()
    }

    /// The labels secrets of the environment carry.
    pub fn secret_labels(&self) -> BTreeMap<String, String> {
        Ownership::scope(&self.instance(), &self.app)
    }

    /// The name of its secret.
    pub fn secret_name(&self) -> String {
        format!("piqueld-secret-{}", self.app)
    }

    /// Its private network.
    pub fn network(&self) -> DesiredNetwork {
        DesiredNetwork {
            name: piqueld_core::DockerNetworkName::parse(docker_resource_name(
                &self.app,
                ResourceKind::Network,
                None,
            ))
            .unwrap(),
            labels: self.labels.clone(),
        }
    }

    /// Its `data` volume.
    pub fn volume(&self) -> DesiredVolume {
        DesiredVolume {
            logical_name: piqueld_core::VolumeName::parse("data").unwrap(),
            name: piqueld_core::DockerVolumeName::parse(docker_resource_name(
                &self.app,
                ResourceKind::Volume,
                Some("data"),
            ))
            .unwrap(),
            labels: self.labels.clone(),
        }
    }

    /// Its `data` volume, mounted writable at `/data`.
    pub fn data_mount(&self) -> piqueld_core::DesiredMount {
        piqueld_core::DesiredMount {
            volume_name: self.volume().name,
            target: "/data".into(),
            read_only: false,
        }
    }

    /// Its service `name`: one healthy replica of [`IMAGE`] that sleeps,
    /// attached to its network.
    pub fn service(&self, name: &str) -> DesiredService {
        let mut labels = self.labels.clone();
        labels.insert(SERVICE_LABEL.into(), name.into());
        DesiredService {
            link_aliases: BTreeMap::new(),
            secrets: Vec::new(),
            files: Vec::new(),
            logical_name: piqueld_core::ServiceName::parse(name).unwrap(),
            name: piqueld_core::DockerServiceName::parse(docker_resource_name(
                &self.app,
                ResourceKind::Service,
                Some(name),
            ))
            .unwrap(),
            source: ResolvedSource::parse_image(IMAGE, self.image.clone()).unwrap(),
            image: piqueld_core::ImmutableImage::parse(self.image.clone()).unwrap(),
            replicas: 1,
            environment: BTreeMap::new(),
            command: vec!["/bin/sh".into()],
            arguments: vec!["-c".into(), "while true; do sleep 1; done".into()],
            mounts: Vec::new(),
            healthcheck: Some(Self::passing_healthcheck()),
            resources: None,
            networks: vec![self.network().name],
            labels,
            depends_on: Vec::new(),
            rollout: piqueld_core::manifest::ValidatedRollout::default(),
            receives_telemetry: false,
            telemetry: None,
        }
    }

    /// A command health check that passes every second.
    pub fn passing_healthcheck() -> ValidatedHealthCheck {
        ValidatedHealthCheck::Command {
            command: vec!["true".into()],
            interval_seconds: 1,
            timeout_seconds: 1,
            start_period_seconds: 0,
            retries: 3,
        }
    }

    /// The `migrate` job's run by `operation`, derived from `service` as
    /// piqueld derives jobs, running `script` with `volume_access` to the
    /// `data` volume, mounted writable at `/data`.
    pub fn job(
        &self,
        service: &DesiredService,
        operation: &str,
        script: &str,
        volume_access: JobVolumeAccess,
    ) -> piqueld_core::DesiredJobRun {
        let name = piqueld_core::JobName::parse("migrate").unwrap();
        let mut container = service.clone();
        container.mounts = vec![self.data_mount()];
        container.name = piqueld_core::DockerServiceName::for_job(&self.app, &name);
        container.command = vec!["/bin/sh".into(), "-c".into(), script.into()];
        container.arguments.clear();
        container.healthcheck = None;
        container.labels = self.labels.clone();
        container.labels.insert(JOB_LABEL.into(), name.to_string());
        piqueld_core::DesiredJob {
            logical_name: name,
            run: piqueld_core::manifest::JobRun::BeforeRollout,
            timeout_seconds: 60,
            volume_access,
            container,
        }
        .for_operation(operation)
    }
}

/// An adapter and the environment a scenario changes through it.
pub struct Contract<D> {
    docker: D,
    environment: Environment,
}

impl<D: DockerApi> Contract<D> {
    /// A scenario of a new environment in `docker`.
    pub async fn new(docker: D) -> Self {
        let environment = Environment::new(&docker).await;
        Self {
            docker,
            environment,
        }
    }

    /// Ensured resources are observed as desired, with their labels, and the
    /// service converged with healthy running tasks; ensuring them again
    /// changes nothing Docker reports.
    pub async fn ensured_resources_are_observed_and_ensuring_again_changes_nothing(&self) {
        let service = self.ensure_web().await;
        let observed = self.converged(&service).await;
        let network = self.environment.network();
        let volume = self.environment.volume();
        assert!(observed.networks.iter().any(|observed| {
            observed.name == network.name.as_str()
                && observed.labels == network.labels
                && observed.runtime_configuration_matches
        }));
        assert!(observed.volumes.iter().any(|observed| {
            observed.name == volume.name.as_str()
                && observed.labels == volume.labels
                && observed.runtime_configuration_matches
        }));

        self.docker.ensure_network(&network).await.unwrap();
        self.docker.ensure_volume(&volume).await.unwrap();
        self.docker.ensure_service(&service).await.unwrap();
        assert_eq!(self.observe().await, observed);
    }

    /// An updated service is observed with the change once it converged.
    pub async fn service_updates_are_observed(&self) {
        let service = self.ensure_web().await;
        self.converged(&service).await;
        let mut updated = service.clone();
        updated.replicas = 2;
        updated.environment = BTreeMap::from([("MODE".into(), "updated".into())]);
        self.docker.ensure_service(&updated).await.unwrap();
        let observed = self.converged(&updated).await;
        assert_eq!(
            Self::service(&observed, &updated).drift(&service),
            ["replicas", "environment"]
        );
    }

    /// Detaching a network changes only the service's networks. A missing
    /// service or network, or one already detached, counts as detached.
    pub async fn detaching_a_network_changes_only_the_service_networks(&self) {
        let service = self.ensure_web().await;
        self.converged(&service).await;
        let network = self.environment.network();
        let detach = async |service: &str, network: &str| {
            self.docker
                .detach_network(service, network, &self.environment.labels)
                .await
                .unwrap();
        };
        let drift = async || {
            let observed = self.observe().await;
            Self::service(&observed, &service).drift(&service)
        };
        detach(service.name.as_str(), network.name.as_str()).await;
        assert_eq!(drift().await, ["networks"]);
        detach(service.name.as_str(), network.name.as_str()).await;
        detach(service.name.as_str(), "piqueld-missing-network").await;
        detach(
            self.environment.service("missing").name.as_str(),
            network.name.as_str(),
        )
        .await;
        assert_eq!(drift().await, ["networks"]);
    }

    /// A network fails removal, as a retryable request, while a service is
    /// attached to it. Once removed, services and networks are no longer
    /// observed, and removing them again succeeds; while Swarm is still
    /// removing the network, removing it again may fail as a retryable
    /// request.
    pub async fn removing_services_and_networks_is_idempotent(&self) {
        let service = self.ensure_web().await;
        self.converged(&service).await;
        let network = self.environment.network();
        let labels = &self.environment.labels;
        let in_use = self
            .docker
            .remove_network(network.name.as_str(), labels)
            .await
            .expect_err("a network services are attached to stays");
        assert_eq!(
            Self::diagnostic(in_use),
            DiagnosticCode::DockerRequestFailed
        );

        self.docker
            .remove_service(service.name.as_str(), labels)
            .await
            .unwrap();
        self.docker
            .remove_service(service.name.as_str(), labels)
            .await
            .unwrap();
        // Tasks of a removed service leave its networks after they stopped.
        self.until("the network is removed", async || {
            match self
                .docker
                .remove_network(network.name.as_str(), labels)
                .await
            {
                Ok(()) => Some(()),
                Err(error) if !error.is_terminal() => None,
                Err(error) => panic!("removing the network failed: {error:?}"),
            }
        })
        .await;
        // Swarm removes the network in the background. Until it's gone, the
        // Engine still inspects it but fails deleting it as a 500 "not
        // found", which the adapter reports as a retryable request failure;
        // the fake removes it at once.
        if let Err(error) = self
            .docker
            .remove_network(network.name.as_str(), labels)
            .await
        {
            assert_eq!(Self::diagnostic(error), DiagnosticCode::DockerRequestFailed);
        }
        let observed = self
            .until("the network is no longer observed", async || {
                let observed = self.observe().await;
                observed.networks.is_empty().then_some(observed)
            })
            .await;
        assert!(observed.services.is_empty(), "{observed:?}");
        self.docker
            .remove_network(network.name.as_str(), labels)
            .await
            .unwrap();
    }

    /// A secret keeps its owner: another environment can neither take it
    /// over nor remove it, and its owner can remove it once, after which its
    /// name is free.
    pub async fn secrets_are_only_changed_by_their_owner(&self) {
        let name = self.environment.secret_name();
        let names = std::slice::from_ref(&name);
        let owner = self.environment.secret_labels();
        let foreign = Ownership::scope(
            &InstanceId::parse("another-instance").unwrap(),
            &self.environment.app,
        );
        self.docker
            .ensure_secret(&name, b"one", &owner)
            .await
            .unwrap();
        self.docker
            .ensure_secret(&name, b"two", &owner)
            .await
            .unwrap();
        assert!(matches!(
            self.docker.ensure_secret(&name, b"one", &foreign).await,
            Err(DockerError::OwnershipConflict)
        ));
        assert!(matches!(
            self.docker.remove_secrets(names, &foreign).await,
            Err(DockerError::OwnershipConflict)
        ));
        self.docker.remove_secrets(names, &owner).await.unwrap();
        self.docker.remove_secrets(names, &owner).await.unwrap();
        self.docker
            .ensure_secret(&name, b"one", &foreign)
            .await
            .unwrap();
        self.docker.remove_secrets(names, &foreign).await.unwrap();
    }

    /// Resolving a tag the registry doesn't have is rejected, so the
    /// controller doesn't retry it.
    pub async fn missing_images_are_rejected(&self) {
        let error = self
            .docker
            .resolve_image(MISSING_IMAGE)
            .await
            .expect_err("a missing tag doesn't resolve");
        assert_eq!(
            Self::diagnostic(error),
            DiagnosticCode::ImageResolutionRejected
        );
    }

    /// A digest-pinned reference resolves to that digest, in [`IMAGE`]'s
    /// repository as in another one serving the image, as the controller
    /// pulls a stored digest again.
    pub async fn pinned_references_resolve_to_their_digest(&self) {
        let image = &self.environment.image;
        assert_eq!(self.docker.resolve_image(image).await.unwrap(), *image);
        let mirrored = self.mirrored();
        assert_eq!(
            self.docker.resolve_image(&mirrored).await.unwrap(),
            mirrored
        );
    }

    /// An image pulled from two repositories is listed once, with both
    /// repositories' digests. The Engine's containerd image store, which
    /// `just docker-test` doesn't run, lists only one of them.
    pub async fn an_image_in_two_repositories_lists_both_digests(&self) {
        let mirrored = self.docker.resolve_image(&self.mirrored()).await.unwrap();
        let resolved = [self.environment.image.as_str(), mirrored.as_str()];
        let images = self.docker.images().await.unwrap();
        let listed = images
            .iter()
            .map(|image| {
                image
                    .repo_digests
                    .iter()
                    .map(piqueld_core::RepositoryDigest::as_str)
                    .filter(|digest| resolved.contains(digest))
                    .collect::<Vec<_>>()
            })
            .filter(|digests| !digests.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(listed, [resolved]);
    }

    /// A job's run is missing, without output, until its operation starts
    /// it, then finishes with its exit code and output. Another operation's
    /// run replaces it, and removing every run leaves none.
    pub async fn job_runs_report_their_status_and_output_per_operation(&self) {
        self.ensure_storage(&self.environment).await;
        let service = self.environment.service("web");
        let job = |operation| {
            self.environment.job(
                &service,
                operation,
                "echo migrated",
                JobVolumeAccess::ReadOnly,
            )
        };
        let (first, second) = (job("operation-1"), job("operation-2"));
        assert_eq!(
            self.docker.job_status(&first).await.unwrap(),
            JobStatus::Missing
        );
        assert_eq!(
            self.docker.job_output(&first).await.unwrap(),
            JobOutput::default()
        );

        self.docker.start_job(&first).await.unwrap();
        assert_eq!(
            self.finished(&first).await,
            JobStatus::Finished {
                exit_code: Some(0),
                error: None
            }
        );
        assert_eq!(
            self.docker.job_output(&first).await.unwrap(),
            JobOutput {
                chunks: vec![(LogStream::Stdout, b"migrated\n".to_vec())],
                truncated: false,
            }
        );

        self.docker.start_job(&second).await.unwrap();
        assert_eq!(
            self.docker.job_status(&first).await.unwrap(),
            JobStatus::Missing
        );
        self.finished(&second).await;
        self.docker
            .remove_jobs(&self.environment.labels, JobRuns::All)
            .await
            .unwrap();
        assert_eq!(
            self.docker.job_status(&second).await.unwrap(),
            JobStatus::Missing
        );
    }

    /// Observing an environment reports none of another's resources.
    pub async fn observations_report_only_their_environment(&self) {
        let other = Environment::new(&self.docker).await;
        for environment in [&self.environment, &other] {
            self.ensure_storage(environment).await;
        }
        let observed = self.observe().await;
        let names = |observed: &ObservedApplication| {
            observed
                .networks
                .iter()
                .map(|network| network.name.clone())
                .chain(observed.volumes.iter().map(|volume| volume.name.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(&observed),
            [
                self.environment.network().name.to_string(),
                self.environment.volume().name.to_string()
            ]
        );
    }

    /// Ensures the network and volume of `environment`.
    async fn ensure_storage(&self, environment: &Environment) {
        self.docker
            .ensure_network(&environment.network())
            .await
            .unwrap();
        self.docker
            .ensure_volume(&environment.volume())
            .await
            .unwrap();
    }

    /// Ensures the environment's network, volume, and `web` service.
    async fn ensure_web(&self) -> DesiredService {
        let service = self.environment.service("web");
        self.ensure_storage(&self.environment).await;
        self.docker.ensure_service(&service).await.unwrap();
        service
    }

    /// Waits until `service` is observed as desired and converged, with its
    /// replicas running healthy, and returns the observation.
    async fn converged(&self, service: &DesiredService) -> ObservedApplication {
        self.until("the service converges", async || {
            let observed = self.observe().await;
            let current = observed
                .services
                .iter()
                .find(|observed| observed.name == service.name.as_str())?;
            let running = current
                .tasks
                .iter()
                .filter(|task| task.desired_running)
                .collect::<Vec<_>>();
            (current.matches(service)
                && current.convergence == Convergence::Converged
                && running.len() == usize::from(service.replicas)
                && running
                    .iter()
                    .all(|task| task.state == TaskState::Running && task.healthy == Some(true)))
            .then_some(observed)
        })
        .await
    }

    /// Waits until `job` stopped and returns its status.
    async fn finished(&self, job: &piqueld_core::DesiredJobRun) -> JobStatus {
        self.until("the job finishes", async || {
            match self.docker.job_status(job).await.unwrap() {
                JobStatus::Running { .. } => None,
                status => Some(status),
            }
        })
        .await
    }

    /// Checks `check` until it returns `Some`, within [`CONVERGENCE`], and
    /// fails as not reaching `expected` otherwise. Each adapter changes
    /// Docker through this, the Engine eventually and the fake at once.
    async fn until<T>(&self, expected: &str, check: impl AsyncFnMut() -> Option<T>) -> T {
        eventually(CONVERGENCE, POLL, check)
            .await
            .unwrap_or_else(|| panic!("timed out waiting until {expected}"))
    }

    /// The environment's image, pinned in [`MIRROR`].
    fn mirrored(&self) -> String {
        let (_, digest) = self.environment.image.split_once('@').unwrap();
        format!("{MIRROR}@{digest}")
    }

    /// The environment as Docker reports it.
    async fn observe(&self) -> ObservedApplication {
        self.docker.observe(&self.environment.app).await.unwrap()
    }

    /// `service` in `observed`.
    fn service<'a>(
        observed: &'a ObservedApplication,
        service: &DesiredService,
    ) -> &'a ObservedService {
        observed
            .services
            .iter()
            .find(|observed| observed.name == service.name.as_str())
            .unwrap_or_else(|| panic!("{} is observed", service.name))
    }

    /// How the controller classifies `error`.
    fn diagnostic(error: DockerError) -> DiagnosticCode {
        OperationError::from(error).diagnostic_code()
    }
}
