//! An in-memory Docker engine for tests and the browser fixture, built with
//! the `fake-docker` feature. It runs every service as declared, healthy and
//! converged, unless a test arms one of its failure knobs.

use super::{
    BollardDocker, DockerApi, DockerError, ImageBuild, ImageSource, JobOutput, JobRuns, JobStatus,
    LocalImage, ResourceKind, SwarmState, resolve_image_digest,
};
use async_trait::async_trait;
use futures_util::StreamExt as _;
use piqueld_core::resource::{
    APPLICATION_LABEL, Convergence, DesiredService, INSTANCE_LABEL, MANAGED_LABEL,
    ObservedApplication, ObservedNetwork, ObservedService, ObservedTask, ObservedVolume,
    SERVICE_LABEL, SPEC_HASH_LABEL, TaskState, image_repository,
};
use piqueld_core::{EnvironmentId, InstanceId, Sha256Digest};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex;

/// The engine's state, shared by its clones. Each knob is off by default.
#[derive(Clone, Default)]
pub struct FakeDocker {
    /// Every service, network, and volume the engine runs.
    pub observed: Arc<Mutex<ObservedApplication>>,
    /// Secrets by Docker secret name.
    pub secrets: Arc<Mutex<BTreeMap<String, Secret>>>,
    /// Fails removing secrets.
    pub fail_secret_removal: Arc<AtomicBool>,
    /// The registry images resolve and pull from.
    pub registry: Arc<Mutex<RegistryState>>,
    /// Refuses removing networks as an ownership conflict.
    pub deny_network_removal: Arc<AtomicBool>,
    /// Fails observations as if Docker were unavailable.
    pub fail_observations: Arc<AtomicBool>,
    /// Leaves ensured services updating.
    pub stall_convergence: Arc<AtomicBool>,
    /// Fails ensured services' rollouts.
    pub fail_convergence: Arc<AtomicBool>,
    /// Logical services that stay updating for this many observations after an ensure.
    pub slow_services: Arc<Mutex<BTreeMap<String, usize>>>,
    /// Service ensures and convergences as `ensure name` / `converged name`, in order.
    pub rollout: Arc<Mutex<Vec<String>>>,
    /// Reports the Swarm as incompatible.
    pub incompatible_swarm: Arc<AtomicBool>,
    /// Holds resolutions of `/slow:` images until released.
    pub resolution_gate: Option<Arc<Gate>>,
    /// Counts service, network, volume, and job mutations.
    pub mutations: Arc<Probe>,
    /// Counts image resolutions.
    pub images: Arc<Probe>,
    /// Counts observations.
    pub observations: Arc<Probe>,
    /// The exit code job runs report once finished.
    pub job_exit: Arc<AtomicI64>,
    /// Every job started, in start order.
    pub job_starts: Arc<Mutex<Vec<JobStart>>>,
    /// Job runs by Docker service name.
    pub jobs: Arc<Mutex<BTreeMap<String, JobRunState>>>,
    /// Keeps job runs running.
    pub hold_jobs: Arc<AtomicBool>,
    /// Fails status checks of started runs.
    pub fail_job_status: Arc<AtomicBool>,
    /// Fails the next job start after creating the run, like a lost response.
    pub lose_job_start: Arc<AtomicBool>,
    /// Rejects starting the named job as an ownership conflict.
    pub conflicting_job: Arc<Mutex<Option<String>>>,
    /// Fails reading job output.
    pub fail_job_output: Arc<AtomicBool>,
    /// Reports job output as truncated.
    pub truncate_job_output: Arc<AtomicBool>,
    /// Fails removing an operation's own run; stale-run removal still works.
    pub fail_job_cleanup: Arc<AtomicBool>,
    /// Blocks removing an operation's own run while held.
    pub hold_job_cleanup: Arc<Mutex<()>>,
    /// Counts removals of an operation's own run, including held ones.
    pub job_cleanups: Arc<AtomicUsize>,
    /// Blocks reading a run's output while held.
    pub hold_job_output: Arc<Mutex<()>>,
    /// Counts reads of a run's output, including held ones.
    pub job_output_reads: Arc<AtomicUsize>,
    /// Blocks waiting for removed job runs to stop while held.
    pub hold_job_stop: Arc<Mutex<()>>,
    /// Counts waits for removed job runs to stop, including held ones.
    pub job_stop_waits: Arc<AtomicUsize>,
    /// The engine's images; builds and pulls add theirs.
    pub local_images: Arc<Mutex<Vec<LocalImage>>>,
    /// Holds builds once their image exists, until released.
    pub build_gate: Option<Arc<Gate>>,
}

/// A Docker secret's value and the labels of the environment that owns it.
#[derive(Clone, Debug)]
pub struct Secret {
    /// The value it was created with; secrets are immutable.
    pub value: Vec<u8>,
    /// Ownership labels it was created with.
    pub labels: BTreeMap<String, String>,
}

/// A job run, as its Swarm job service records it.
#[derive(Clone, Debug)]
pub struct JobRunState {
    /// The operation that started it.
    pub operation: String,
    /// The exit code it reports once finished.
    pub exit_code: i64,
    /// When it times out.
    pub deadline: std::time::SystemTime,
    /// The labels of the job service, as the run's `DesiredJobRun` sets them.
    pub labels: BTreeMap<String, String>,
}

impl JobRunState {
    /// The run of `job` that exits with `exit_code`, started now.
    #[must_use]
    pub fn of(job: &piqueld_core::DesiredJobRun, exit_code: i64) -> Self {
        Self {
            operation: job.operation().into(),
            exit_code,
            deadline: std::time::SystemTime::now()
                + Duration::from_secs(u64::from(job.timeout_seconds)),
            labels: job.container.labels.clone(),
        }
    }
}

/// A started job with the services, networks, and volumes it could see.
#[derive(Debug, PartialEq, Eq)]
pub struct JobStart {
    /// The job's logical name.
    pub job: String,
    /// Services running when it started.
    pub services: usize,
    /// Networks existing when it started.
    pub networks: usize,
    /// Volumes existing when it started.
    pub volumes: usize,
}

/// Holds a fake Docker call once it is entered, until released.
#[derive(Default)]
pub struct Gate {
    /// Notified when a call enters the gate.
    pub entered: tokio::sync::Notify,
    /// Notify to let the waiting call through.
    pub release: tokio::sync::Notify,
}

impl Gate {
    async fn pass(&self) {
        self.entered.notify_one();
        self.release.notified().await;
    }
}

/// Counts the concurrent and total calls of one kind of Docker request.
#[derive(Default)]
pub struct Probe {
    active: AtomicUsize,
    /// The most calls that ran at once.
    pub maximum: AtomicUsize,
    /// Every call so far.
    pub total: AtomicUsize,
}

/// One call counted by [`Probe::enter`], until dropped.
struct ProbeGuard<'a>(&'a Probe);

impl Drop for ProbeGuard<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Probe {
    async fn enter(&self) -> ProbeGuard<'_> {
        let guard = ProbeGuard(self);
        self.total.fetch_add(1, Ordering::SeqCst);
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(active, Ordering::SeqCst);
        tokio::task::yield_now().await;
        guard
    }
}

/// The fake registry. Programmatic hook: the tag is re-pointed after this
/// many remaining pulls of tags; pulls of digest-pinned references never
/// re-point anything.
#[derive(Default)]
pub struct RegistryState {
    /// Pulls so far, by reference.
    pub pulls: BTreeMap<String, u64>,
    /// Digests tags point at by reference, besides the base digest. A
    /// digest-pinned reference always points at its own digest.
    pub digests: BTreeMap<String, String>,
    /// References the registry doesn't have; every other one exists.
    pub missing: BTreeSet<String>,
    flips_remaining: usize,
}

impl RegistryState {
    fn base_digest() -> String {
        "a".repeat(64)
    }

    /// The digest a flipped tag points at.
    #[must_use]
    pub fn flipped_digest() -> String {
        "b".repeat(64)
    }

    fn digest(&self, reference: &str) -> String {
        match reference.split_once("@sha256:") {
            Some((_, pinned)) => pinned.to_owned(),
            None => self
                .digests
                .get(reference)
                .cloned()
                .unwrap_or_else(Self::base_digest),
        }
    }

    fn pull(&mut self, reference: &str) {
        *self.pulls.entry(reference.to_owned()).or_insert(0) += 1;
        if self.flips_remaining > 0 && !reference.contains('@') {
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

/// An [`ImageSource`] view over the fake registry, reporting repo digests
/// as the real engine does: by the repository's short name on Docker Hub
/// (`nginx@sha256:…`, `user/app@sha256:…`), by its full name elsewhere.
struct RegistryView {
    registry: Arc<Mutex<RegistryState>>,
}

#[async_trait]
impl ImageSource for RegistryView {
    async fn repo_digests(&self, reference: &str) -> Result<Option<Vec<String>>, DockerError> {
        let state = self.registry.lock().await;
        if state.missing.contains(reference) {
            return Ok(None);
        }
        let repository =
            image_repository(reference).ok_or(DockerError::Request("image reference"))?;
        let repository = repository
            .strip_prefix("docker.io/")
            .map_or(repository.as_str(), |path| {
                path.strip_prefix("library/").unwrap_or(path)
            });
        Ok(Some(vec![format!(
            "{repository}@sha256:{}",
            state.digest(reference)
        )]))
    }

    /// Fails a missing reference as the registry's 404, as the Engine does.
    async fn pull(&self, reference: &str) -> Result<(), DockerError> {
        let mut state = self.registry.lock().await;
        if state.missing.contains(reference) {
            return Err(DockerError::ImageResolutionSource {
                operation: "pull image",
                source: bollard::errors::Error::DockerResponseServerError {
                    status_code: 404,
                    message: format!("manifest for {reference} not found: manifest unknown"),
                },
            });
        }
        state.pull(reference);
        Ok(())
    }
}

impl FakeDocker {
    /// Arms the registry to re-point the tag after each remaining pull.
    pub async fn arm_tag_flips(&self, flips: usize) {
        self.registry.lock().await.flips_remaining = flips;
    }

    /// Adds `image`, or merges its repository digests into the image with
    /// its ID, as the Engine lists an image pulled from several
    /// repositories once with each repository's digest.
    async fn add_image(&self, image: LocalImage) {
        let mut images = self.local_images.lock().await;
        match images.iter_mut().find(|existing| existing.id == image.id) {
            Some(existing) => {
                for digest in image.repo_digests {
                    if !existing.repo_digests.contains(&digest) {
                        existing.repo_digests.push(digest);
                    }
                }
            }
            None => images.push(image),
        }
    }

    /// The run of `job` by its operation, if it has one.
    async fn run(&self, job: &piqueld_core::DesiredJobRun) -> Option<JobRunState> {
        self.jobs
            .lock()
            .await
            .get(job.container.name.as_str())
            .filter(|run| run.operation == job.operation())
            .cloned()
    }

    /// Whether any of `services` is attached to the network `name`, as
    /// Docker refuses to remove it then.
    fn attached(services: &[ObservedService], name: &str) -> bool {
        services.iter().any(|service| {
            service
                .networks
                .iter()
                .any(|attachment| attachment.network == name)
        })
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

    /// Whether a secret labelled `observed` belongs to the environment
    /// `expected` names by its managed, instance, and application labels.
    fn scope_matches(
        observed: &BTreeMap<String, String>,
        expected: &BTreeMap<String, String>,
    ) -> bool {
        expected.get(MANAGED_LABEL).map(String::as_str) == Some("true")
            && [MANAGED_LABEL, INSTANCE_LABEL, APPLICATION_LABEL]
                .iter()
                .all(|key| {
                    expected
                        .get(*key)
                        .is_some_and(|value| observed.get(*key) == Some(value))
                })
    }

    /// Refuses changing or removing a resource labelled `existing`, if it
    /// exists, unless `ownership` owns it.
    fn check_owned(
        existing: Option<&BTreeMap<String, String>>,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        match existing {
            Some(labels) if !Self::ownership_matches(labels, ownership) => {
                Err(DockerError::OwnershipConflict)
            }
            _ => Ok(()),
        }
    }

    /// Refuses reusing a network or volume owned by someone else or labelled
    /// as a service, or one Docker configured differently.
    fn check_reusable(
        labels: &BTreeMap<String, String>,
        runtime_configuration_matches: bool,
        desired: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        if !Self::ownership_matches(labels, desired) || labels.contains_key(SERVICE_LABEL) {
            return Err(DockerError::OwnershipConflict);
        }
        if !runtime_configuration_matches {
            return Err(DockerError::ConfigurationConflict);
        }
        Ok(())
    }
}

/// How Docker reports `desired` once it runs as declared: one running,
/// healthy task per replica.
#[must_use]
pub fn observed_service(desired: &DesiredService) -> ObservedService {
    ObservedService {
        secrets: desired.secrets.clone(),
        files: desired
            .files
            .iter()
            .map(piqueld_core::resource::ObservedFile::from)
            .collect(),
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
        networks: desired.network_attachments(),
        labels: desired.labels.clone(),
        rollout: Some(desired.rollout_policy()),
        telemetry: desired.telemetry.clone(),
        runtime_configuration_matches: true,
        tasks: vec![
            ObservedTask {
                state: TaskState::Running,
                healthy: Some(true),
                desired_running: true,
                diagnostic: None,
            };
            usize::from(desired.replicas)
        ],
        convergence: Convergence::Converged,
    }
}

#[async_trait]
impl DockerApi for FakeDocker {
    async fn ensure_secret(
        &self,
        name: &str,
        value: &[u8],
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        let mut secrets = self.secrets.lock().await;
        if let Some(existing) = secrets.get(name) {
            return if Self::scope_matches(&existing.labels, ownership) {
                Ok(())
            } else {
                Err(DockerError::OwnershipConflict)
            };
        }
        secrets.insert(
            name.to_owned(),
            Secret {
                value: value.to_vec(),
                labels: ownership.clone(),
            },
        );
        Ok(())
    }

    async fn remove_secrets(
        &self,
        names: &[String],
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        if self.fail_secret_removal.load(Ordering::SeqCst) {
            return Err(DockerError::Request("remove secrets"));
        }
        let mut secrets = self.secrets.lock().await;
        for name in names {
            if let Some(secret) = secrets.get(name) {
                if !Self::scope_matches(&secret.labels, ownership) {
                    return Err(DockerError::OwnershipConflict);
                }
                secrets.remove(name);
            }
        }
        Ok(())
    }

    async fn application_logs(
        &self,
        _instance: &InstanceId,
        _application: &EnvironmentId,
        _service: Option<&str>,
        _tail: u16,
        _since: u32,
        _stream: Option<piqueld_core::api::LogStream>,
    ) -> Result<piqueld_core::api::ApplicationLogs, DockerError> {
        Ok(piqueld_core::api::ApplicationLogs::default())
    }

    /// The fake runs no containers.
    async fn managed_containers(
        &self,
        _instance: &InstanceId,
    ) -> Result<Vec<super::ManagedContainer>, DockerError> {
        Ok(Vec::new())
    }

    async fn container_usage(
        &self,
        _id: &str,
    ) -> Result<Option<super::ContainerUsage>, DockerError> {
        Ok(None)
    }

    fn container_logs(&self, _id: &str, _since: i32) -> super::ContainerLogs {
        futures_util::stream::empty().boxed()
    }

    /// No container ever starts.
    fn container_starts(&self, _since: i64) -> super::ContainerStarts {
        futures_util::stream::pending().boxed()
    }

    async fn ping(&self) -> Result<(), DockerError> {
        Ok(())
    }

    async fn ensure_swarm(&self, _auto_initialize: bool) -> Result<SwarmState, DockerError> {
        if self.incompatible_swarm.load(Ordering::SeqCst) {
            return Err(DockerError::IncompatibleSwarm);
        }
        Ok(SwarmState::Ready)
    }

    /// Builds image `b`s, unless the Dockerfile says `build-fails`. Build
    /// output is not recorded.
    async fn build_image(
        &self,
        build: &ImageBuild<'_>,
        _log: Option<&crate::build::BuildLog>,
    ) -> Result<Sha256Digest, DockerError> {
        if !build.context.is_dir() {
            return Err(DockerError::Validation("find the build context"));
        }
        let contents = tokio::fs::read_to_string(&build.dockerfile)
            .await
            .map_err(|source| DockerError::RequestSource {
                operation: "read the Dockerfile",
                source: source.into(),
            })?;
        if contents.contains("build-fails") {
            return Err(DockerError::Request("build Docker image"));
        }
        self.registry.lock().await.pull("git-build");
        let id = Sha256Digest::parse(format!("sha256:{}", "b".repeat(64))).map_err(|source| {
            DockerError::RequestSource {
                operation: "name the built image",
                source: source.into(),
            }
        })?;
        self.add_image(LocalImage {
            id: id.clone(),
            repo_digests: Vec::new(),
            labels: BTreeMap::from([
                (MANAGED_LABEL.into(), "true".into()),
                (INSTANCE_LABEL.into(), build.owner.to_string()),
            ]),
            size: 100,
            used: false,
        })
        .await;
        if let Some(gate) = &self.build_gate {
            gate.pass().await;
        }
        Ok(id)
    }
    /// The reconciler never runs commands.
    async fn create_exec(
        &self,
        _instance: &InstanceId,
        _environment: &EnvironmentId,
        _request: &piqueld_core::exec::ExecRequest,
    ) -> Result<Option<super::Exec>, DockerError> {
        Err(DockerError::Unavailable("create exec"))
    }

    async fn run_exec(&self, _exec: &super::Exec, _io: super::ExecIo) -> Result<i64, DockerError> {
        Err(DockerError::Unavailable("run exec"))
    }

    async fn resolve_image(&self, reference: &str) -> Result<String, DockerError> {
        let _probe = self.images.enter().await;
        if reference.contains("/slow:")
            && let Some(gate) = &self.resolution_gate
        {
            gate.pass().await;
        }
        let digest = resolve_image_digest(
            &RegistryView {
                registry: Arc::clone(&self.registry),
            },
            reference,
        )
        .await?;
        let parse = |source: Box<dyn std::error::Error + Send + Sync>| DockerError::RequestSource {
            operation: "record the pulled image",
            source,
        };
        let (_, id) = digest
            .split_once('@')
            .ok_or(DockerError::ImageResolution("split the repository digest"))?;
        self.add_image(LocalImage {
            id: Sha256Digest::parse(id).map_err(|error| parse(error.into()))?,
            repo_digests: vec![
                piqueld_core::RepositoryDigest::parse(digest.clone())
                    .map_err(|error| parse(error.into()))?,
            ],
            labels: BTreeMap::new(),
            size: 50,
            used: false,
        })
        .await;
        Ok(digest)
    }

    async fn images(&self) -> Result<Vec<LocalImage>, DockerError> {
        Ok(self.local_images.lock().await.clone())
    }

    async fn prune_health_probes(&self) -> Result<(), DockerError> {
        Ok(())
    }

    async fn prune_files(&self, _ownership: &BTreeMap<String, String>) -> Result<(), DockerError> {
        Ok(())
    }

    async fn remove_image(
        &self,
        instance: &InstanceId,
        id: &Sha256Digest,
    ) -> Result<(), DockerError> {
        let mut images = self.local_images.lock().await;
        if images
            .iter()
            .any(|image| image.id == *id && !image.built_by(instance))
        {
            return Err(DockerError::OwnershipConflict);
        }
        images.retain(|image| image.id != *id);
        Ok(())
    }

    async fn observe(
        &self,
        application: &EnvironmentId,
    ) -> Result<ObservedApplication, DockerError> {
        let _probe = self.observations.enter().await;
        if self.fail_observations.load(Ordering::SeqCst) {
            return Err(DockerError::Unavailable("observing application"));
        }
        let mut observed = self.observed.lock().await;
        let mut slow = self.slow_services.lock().await;
        for service in &mut observed.services {
            let Some(name) = service.labels.get(SERVICE_LABEL) else {
                continue;
            };
            if let Some(remaining) = slow.get_mut(name)
                && service.convergence == Convergence::Updating
            {
                *remaining = remaining.saturating_sub(1);
                if *remaining == 0 {
                    service.convergence = Convergence::Converged;
                    self.rollout.lock().await.push(format!("converged {name}"));
                }
            }
        }
        drop(slow);
        let mut observed = observed.clone();
        for network in &mut observed.networks {
            network.in_use = Self::attached(&observed.services, &network.name);
        }
        // As the adapter observes: see `BollardDocker::relevant`.
        let relevant = |name: &str, labels: &BTreeMap<String, String>| {
            BollardDocker::relevant(name, labels, application)
        };
        observed
            .services
            .retain(|service| relevant(&service.name, &service.labels));
        observed
            .volumes
            .retain(|volume| relevant(&volume.name, &volume.labels));
        (observed.networks, observed.linked_networks) = observed
            .networks
            .into_iter()
            .filter(|network| {
                relevant(&network.name, &network.labels)
                    || BollardDocker::linked(&network.labels, application)
            })
            .partition(|network| relevant(&network.name, &network.labels));
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
            return Self::check_reusable(
                &existing.labels,
                existing.runtime_configuration_matches,
                &desired.labels,
            );
        }
        observed.networks.push(ObservedNetwork {
            name: desired.name.to_string(),
            runtime_configuration_matches: true,
            labels: desired.labels.clone(),
            in_use: false,
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
            return Self::check_reusable(
                &existing.labels,
                existing.runtime_configuration_matches,
                &desired.labels,
            );
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
        let existing = observed
            .services
            .iter()
            .find(|service| service.name == desired.name.as_str());
        Self::check_owned(existing.map(|service| &service.labels), &desired.labels)?;
        observed
            .services
            .retain(|service| service.name != desired.name.as_str());
        let mut service = observed_service(desired);
        let logical = desired.logical_name.to_string();
        if self.fail_convergence.load(Ordering::SeqCst) {
            service.convergence = Convergence::Failed;
        } else if self.stall_convergence.load(Ordering::SeqCst)
            || self.slow_services.lock().await.get(&logical) > Some(&0)
        {
            service.convergence = Convergence::Updating;
        }
        observed.services.push(service);
        self.rollout.lock().await.push(format!("ensure {logical}"));
        Ok(())
    }

    async fn remove_service(
        &self,
        name: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        let _probe = self.mutations.enter().await;
        let mut observed = self.observed.lock().await;
        let existing = observed
            .services
            .iter()
            .find(|service| service.name == name);
        Self::check_owned(existing.map(|service| &service.labels), ownership)?;
        observed.services.retain(|service| service.name != name);
        Ok(())
    }

    async fn detach_network(
        &self,
        service: &str,
        network: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        let _probe = self.mutations.enter().await;
        let mut observed = self.observed.lock().await;
        let Some(existing) = observed
            .services
            .iter_mut()
            .find(|existing| existing.name == service)
        else {
            return Ok(());
        };
        Self::check_owned(Some(&existing.labels), ownership)?;
        existing
            .networks
            .retain(|attachment| attachment.network != network);
        Ok(())
    }

    async fn start_job(&self, job: &piqueld_core::DesiredJobRun) -> Result<(), DockerError> {
        let _probe = self.mutations.enter().await;
        if self.conflicting_job.lock().await.as_deref() == Some(job.logical_name.as_str()) {
            return Err(DockerError::OwnershipConflict);
        }
        let observed = self.observed.lock().await;
        self.job_starts.lock().await.push(JobStart {
            job: job.logical_name.to_string(),
            services: observed
                .services
                .iter()
                .filter(|service| service.replicas > 0)
                .count(),
            networks: observed.networks.len(),
            volumes: observed.volumes.len(),
        });
        self.rollout
            .lock()
            .await
            .push(format!("job {}", job.logical_name));
        self.jobs.lock().await.insert(
            job.container.name.to_string(),
            JobRunState::of(job, self.job_exit.load(Ordering::SeqCst)),
        );
        if self.lose_job_start.swap(false, Ordering::SeqCst) {
            return Err(DockerError::Request("start job"));
        }
        Ok(())
    }

    async fn job_status(
        &self,
        job: &piqueld_core::DesiredJobRun,
    ) -> Result<JobStatus, DockerError> {
        let Some(JobRunState {
            exit_code,
            deadline,
            ..
        }) = self.run(job).await
        else {
            return Ok(JobStatus::Missing);
        };
        if self.fail_job_status.load(Ordering::SeqCst) {
            return Err(DockerError::Request("inspect job"));
        }
        if self.hold_jobs.load(Ordering::SeqCst) {
            return Ok(JobStatus::Running {
                deadline: Some(deadline),
            });
        }
        Ok(JobStatus::Finished {
            exit_code: Some(exit_code),
            error: (exit_code != 0).then(|| format!("task: non-zero exit ({exit_code})")),
        })
    }

    /// Every run prints `migrated`; an operation without a run has no output.
    async fn job_output(
        &self,
        job: &piqueld_core::DesiredJobRun,
    ) -> Result<JobOutput, DockerError> {
        self.job_output_reads.fetch_add(1, Ordering::SeqCst);
        drop(self.hold_job_output.lock().await);
        if self.fail_job_output.load(Ordering::SeqCst) {
            return Err(DockerError::Request("read job output"));
        }
        if self.run(job).await.is_none() {
            return Ok(JobOutput::default());
        }
        Ok(JobOutput {
            chunks: vec![(piqueld_core::api::LogStream::Stdout, b"migrated\n".to_vec())],
            truncated: self.truncate_job_output.load(Ordering::SeqCst),
        })
    }

    /// Removes the selected runs `ownership`'s environment owns, by the
    /// adapter's job ownership check; other runs are left alone.
    async fn remove_jobs(
        &self,
        ownership: &BTreeMap<String, String>,
        runs: JobRuns<'_>,
    ) -> Result<(), DockerError> {
        if matches!(runs, JobRuns::Of(_)) {
            self.job_cleanups.fetch_add(1, Ordering::SeqCst);
            let _held = self.hold_job_cleanup.lock().await;
            if self.fail_job_cleanup.load(Ordering::SeqCst) {
                return Err(DockerError::Request("remove jobs"));
            }
        }
        self.jobs.lock().await.retain(|name, run| {
            !(runs.selects(Some(&run.operation))
                && BollardDocker::check_ownership(
                    run.labels.clone().into_iter().collect(),
                    ownership,
                    ResourceKind::Job,
                    name,
                )
                .is_ok())
        });
        Ok(())
    }

    async fn wait_jobs_stopped(
        &self,
        _ownership: &BTreeMap<String, String>,
        _runs: JobRuns<'_>,
    ) -> Result<(), DockerError> {
        self.job_stop_waits.fetch_add(1, Ordering::SeqCst);
        let _held = self.hold_job_stop.lock().await;
        Ok(())
    }

    async fn wait_volumes_released(
        &self,
        _job: &piqueld_core::DesiredJobRun,
    ) -> Result<(), DockerError> {
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
        let existing = observed
            .networks
            .iter()
            .find(|network| network.name == name);
        Self::check_owned(existing.map(|network| &network.labels), ownership)?;
        if Self::attached(&observed.services, name) {
            return Err(DockerError::Request("remove a network in use"));
        }
        observed.networks.retain(|network| network.name != name);
        Ok(())
    }

    async fn remove_volume(
        &self,
        name: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        let _probe = self.mutations.enter().await;
        let mut observed = self.observed.lock().await;
        let existing = observed.volumes.iter().find(|volume| volume.name == name);
        Self::check_owned(existing.map(|volume| &volume.labels), ownership)?;
        observed.volumes.retain(|volume| volume.name != name);
        Ok(())
    }
}
