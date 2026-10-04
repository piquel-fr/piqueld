//! Docker Engine/Swarm boundary.
//!
//! The reconciler only depends on [`DockerApi`]. Bollard is deliberately kept at
//! this edge so unit tests can use a deterministic in-memory implementation.
use async_trait::async_trait;
use bollard::{
    Docker,
    models::{
        HealthConfig, Ipam, Limit, Mount, MountTypeEnum, NetworkAttachmentConfig,
        NetworkCreateRequest, ServiceSpec, ServiceSpecMode, ServiceSpecModeReplicated,
        ServiceSpecUpdateConfig, ServiceSpecUpdateConfigFailureActionEnum,
        ServiceSpecUpdateConfigOrderEnum, SwarmInitRequest, TaskSpec, TaskSpecContainerSpec,
        TaskSpecResources, TaskSpecRestartPolicy, TaskSpecRestartPolicyConditionEnum,
        VolumeCreateOptions,
    },
    query_parameters::{
        CreateImageOptionsBuilder, InspectContainerOptionsBuilder, InspectNetworkOptions,
        InspectServiceOptions, ListNetworksOptionsBuilder, ListNodesOptions,
        ListServicesOptionsBuilder, ListTasksOptionsBuilder, ListVolumesOptionsBuilder,
    },
};
use futures_util::{StreamExt, TryStreamExt, stream};
use piqueld_core::manifest::{HealthCheck, ResourceLimits};
use piqueld_core::resource::{
    APPLICATION_LABEL, Convergence, DesiredNetwork, DesiredService, DesiredVolume,
    INGRESS_PROXIES_ENV, INSTANCE_LABEL, MANAGED_LABEL, NetworkAttachment, ObservedMount,
    ObservedNetwork, ObservedService, ObservedTask, ObservedVolume, SERVICE_LABEL, TaskDiagnostic,
    TaskState, image_repository,
};
use piqueld_core::{
    DockerNetworkName, EnvironmentId, InstanceId, ObservedApplication, ResourceKind,
    docker_resource_name, docker_resource_readable_prefix,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
    sync::Arc,
    time::Duration,
};

/// Docker represents health-check and Swarm policy durations in nanoseconds.
const NANOSECONDS_PER_SECOND: i64 = 1_000_000_000;
/// Docker expresses CPU limits in billionths of a CPU (`NanoCPUs`).
///
/// ```text
/// 250 millicores → 250_000_000 NanoCPUs
/// ```
const NANO_CPUS_PER_MILLICORE: i64 = 1_000_000;
/// Delay Swarm waits before restarting an exited task.
const RESTART_DELAY: i64 = 2 * NANOSECONDS_PER_SECOND;
/// Consecutive failed health probes before a container is marked unhealthy.
const HEALTH_RETRIES: i64 = 3;

/// Concurrent per-service inspections performed during one observation.
const OBSERVATION_INSPECT_CONCURRENCY: usize = 8;

/// Resolution attempts made before an image tag is declared unstable.
const IMAGE_RESOLVE_ATTEMPTS: usize = 3;
/// Pause between resolution attempts after a suspected concurrent tag flip.
const IMAGE_RESOLVE_RETRY_DELAY: Duration = Duration::from_millis(100);

#[derive(Clone)]
/// A shared connection to the Docker Engine.
pub struct BollardDocker {
    /// Typed Bollard client used for most Engine requests.
    docker: Arc<Docker>,
    /// Engine socket path, reused for raw service requests and `docker build`.
    socket: Arc<Path>,
}

mod timeout;
pub(crate) use timeout::DockerTimeout;
mod engine;
mod exec;
pub use exec::{Exec, ExecIo};
mod limited;
mod logs;
pub(crate) use limited::LimitedDocker;
mod errors;
mod identity;
mod jobs;
mod observation;
mod policy;
mod resources;
mod secrets;
mod spec;
pub use errors::DockerError;

/// Progress of one operation's run of a one-shot job.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JobStatus {
    /// The operation has no run of the job; any run of another operation is ignored.
    Missing,
    /// The run has not finished.
    Running,
    /// The run finished.
    Finished {
        /// Container exit code, absent when Docker rejected or stopped the task
        /// without reporting one.
        exit_code: Option<i64>,
        /// Docker's explanation when the task did not complete successfully.
        error: Option<String>,
    },
}

/// Output of one job run, merging consecutive chunks of the same stream.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct JobOutput {
    /// Output in the order Docker returned it.
    pub chunks: Vec<(piqueld_core::api::LogStream, Vec<u8>)>,
    /// Whether output past the read limit was dropped.
    pub truncated: bool,
}

/// Selects job services by the operation that started them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobRuns<'a> {
    /// Every job run of the application.
    All,
    /// Runs started by this operation.
    Of(&'a str),
    /// Runs started by any other operation.
    Except(&'a str),
}

impl JobRuns<'_> {
    /// Returns whether a run started by `operation` is selected.
    #[must_use]
    pub fn selects(self, operation: Option<&str>) -> bool {
        match self {
            Self::All => true,
            Self::Of(selected) => operation == Some(selected),
            Self::Except(kept) => operation != Some(kept),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// The result of checking or initializing the local Swarm.
pub enum SwarmState {
    /// The local engine was already a compatible Swarm manager.
    Ready,
    /// The local engine was initialized as a compatible Swarm manager.
    Initialized,
}

#[async_trait]
/// The runtime operations required by the reconciler.
///
/// Methods have no default implementations: every adapter states its own
/// behavior, so a new method cannot be silently missing from one of them.
pub trait DockerApi: Send + Sync + 'static {
    /// Reads a bounded historical log window without storing it in piqueld.
    async fn application_logs(
        &self,
        instance: &InstanceId,
        application: &EnvironmentId,
        service: Option<&str>,
        tail: u16,
        since: u32,
        stream: Option<piqueld_core::api::LogStream>,
    ) -> Result<piqueld_core::api::ApplicationLogs, DockerError>;

    /// Probes Engine reachability independently of Swarm configuration.
    async fn ping(&self) -> Result<(), DockerError>;

    /// Ensures that Docker is an active, compatible Swarm manager.
    async fn ensure_swarm(&self, auto_initialize: bool) -> Result<SwarmState, DockerError>;
    /// Pulls an image reference and returns its immutable repository digest.
    async fn resolve_image(&self, reference: &str) -> Result<String, DockerError>;
    /// Builds local Docker inputs into an immutable image, streaming output
    /// into `log` when given.
    async fn build_image_recorded(
        &self,
        dockerfile: &Path,
        context: &Path,
        log: Option<&crate::build::BuildLog>,
    ) -> Result<piqueld_core::resource::Sha256Digest, DockerError>;
    /// Provisions an immutable secret with the expected application ownership.
    async fn ensure_secret(
        &self,
        name: &str,
        value: &[u8],
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError>;
    /// Removes only secrets matching the expected application ownership.
    async fn remove_secrets(
        &self,
        names: &[String],
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError>;
    /// Creates a command in one running task of an owned service.
    /// Returns `None` when the service has no running task.
    async fn create_exec(
        &self,
        instance: &InstanceId,
        environment: &EnvironmentId,
        request: &piqueld_core::exec::ExecRequest,
    ) -> Result<Option<Exec>, DockerError>;
    /// Streams a created command until it exits and returns its exit code.
    async fn run_exec(&self, exec: &Exec, io: ExecIo) -> Result<i64, DockerError>;
    /// Builds a local image without persisting output.
    async fn build_image(
        &self,
        dockerfile: &Path,
        context: &Path,
    ) -> Result<piqueld_core::resource::Sha256Digest, DockerError>;
    /// Reads the resources managed for one application.
    async fn observe(
        &self,
        application: &EnvironmentId,
    ) -> Result<ObservedApplication, DockerError>;
    /// Creates or verifies a managed network.
    async fn ensure_network(&self, desired: &DesiredNetwork) -> Result<(), DockerError>;
    /// Creates or verifies a managed volume.
    async fn ensure_volume(&self, desired: &DesiredVolume) -> Result<(), DockerError>;
    /// Creates or updates a managed service.
    async fn ensure_service(&self, desired: &DesiredService) -> Result<(), DockerError>;
    /// Removes a managed service after rechecking its ownership.
    async fn remove_service(
        &self,
        name: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError>;
    /// Starts the run of `job`. Any existing service of the same job, including
    /// a finished run of the same operation, is removed first, and its
    /// container has stopped before the new run is created, so two runs never
    /// overlap.
    async fn start_job(&self, job: &piqueld_core::DesiredJobRun) -> Result<(), DockerError>;
    /// Reads the progress of the run of `job` by its operation.
    async fn job_status(&self, job: &piqueld_core::DesiredJobRun)
    -> Result<JobStatus, DockerError>;
    /// Reads the run's bounded output so far.
    async fn job_output(&self, job: &piqueld_core::DesiredJobRun)
    -> Result<JobOutput, DockerError>;
    /// Removes the selected job services owned by an application and waits
    /// until their containers have stopped. Waiting is based on the containers
    /// still running, so a retry after a failed wait waits again. Observation
    /// never reports jobs, so this is the only cleanup path for them.
    async fn remove_jobs(
        &self,
        ownership: &BTreeMap<String, String>,
        runs: JobRuns<'_>,
    ) -> Result<(), DockerError>;
    /// Removes a managed private network after rechecking its ownership.
    async fn remove_network(
        &self,
        name: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError>;
}

#[async_trait]
/// The image-registry operations required to resolve a reference to a digest.
///
/// Keeping this seam narrow lets the tag-stability verification run against
/// deterministic in-memory doubles as well as the real Bollard connection.
pub trait ImageSource: Send + Sync {
    /// Lists the repository digests recorded locally for a reference, or
    /// `None` when the reference is unknown to the engine.
    async fn repo_digests(&self, reference: &str) -> Result<Option<Vec<String>>, DockerError>;
    /// Pulls the reference into the local image store.
    async fn pull(&self, reference: &str) -> Result<(), DockerError>;
}

/// Resolves an image reference to the repository digest recorded under its tag.
///
/// The pull races the tag's mutability, so the digests matching the requested
/// repository are captured before and after the pull and must still overlap;
/// otherwise the whole resolution restarts until [`IMAGE_RESOLVE_ATTEMPTS`] is
/// exhausted. A reference previously unknown to the engine has no prior digests
/// to protect, so its first successful pull is accepted directly.
///
/// # Errors
///
/// Returns the sanitized image-resolution error class when the engine fails,
/// the pull never produces a matching digest, or the tag keeps flipping.
///
/// ```text
/// ghcr.io/example/notes:1.4        → ghcr.io/example/notes@sha256:<64 hex>
/// ghcr.io/example/notes@sha256:abc → the local repo digest equal to sha256:abc
/// ```
pub async fn resolve_image_digest(
    source: &(impl ImageSource + ?Sized),
    reference: &str,
) -> Result<String, DockerError> {
    let repository = image_repository(reference)
        .ok_or(DockerError::ImageResolution("parse image repository"))?;
    let requested_digest = reference.split_once('@').map(|(_, digest)| digest);
    for attempt in 0..IMAGE_RESOLVE_ATTEMPTS {
        let before = matching_repo_digests(source, reference, &repository).await?;
        source.pull(reference).await?;
        let after = matching_repo_digests(source, reference, &repository).await?;
        // Digest-pinned references must resolve to exactly that digest; tags
        // must resolve to a digest that was already present before the pull (any
        // digest when none was present yet).
        let Some(digest) = after
            .iter()
            .find(|digest| {
                requested_digest.map_or_else(
                    || before.is_empty() || before.contains(*digest),
                    |requested| {
                        digest
                            .split_once('@')
                            .is_some_and(|(_, resolved)| resolved == requested)
                    },
                )
            })
            .cloned()
        else {
            if after.is_empty() {
                return Err(DockerError::ImageResolution("find repository digest"));
            }
            if attempt + 1 < IMAGE_RESOLVE_ATTEMPTS {
                tokio::time::sleep(IMAGE_RESOLVE_RETRY_DELAY).await;
            }
            continue;
        };
        return Ok(digest);
    }
    Err(DockerError::ImageResolution("confirm stable image digest"))
}

/// Returns the locally recorded, well-formed repository digests of `reference`
/// that belong to `repository`, ignoring digests of other repositories that
/// share the same image ID.
async fn matching_repo_digests(
    source: &(impl ImageSource + ?Sized),
    reference: &str,
    repository: &str,
) -> Result<BTreeSet<String>, DockerError> {
    Ok(source
        .repo_digests(reference)
        .await?
        .unwrap_or_default()
        .into_iter()
        .filter(|digest| {
            image_repository(digest).as_deref() == Some(repository)
                && BollardDocker::valid_digest(digest)
        })
        .collect())
}
