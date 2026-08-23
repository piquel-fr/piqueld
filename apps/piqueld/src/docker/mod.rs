//! Docker Engine/Swarm boundary.
//!
//! The reconciler only depends on [`DockerApi`]. Bollard is deliberately kept at
//! this edge so unit tests can use a deterministic in-memory implementation.
use crate::operations::OperationError;
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
        CreateImageOptionsBuilder, InspectNetworkOptions, InspectServiceOptions,
        ListNetworksOptionsBuilder, ListNodesOptions, ListServicesOptionsBuilder,
        ListTasksOptionsBuilder, ListVolumesOptionsBuilder,
    },
};
use futures_util::{StreamExt, TryStreamExt, stream};
use piqueld_core::manifest::{HealthCheck, ResourceLimits};
use piqueld_core::resource::{
    APPLICATION_LABEL, Convergence, DesiredMount, DesiredNetwork, DesiredService, DesiredVolume,
    INSTANCE_LABEL, MANAGED_LABEL, ObservedNetwork, ObservedService, ObservedTask, ObservedVolume,
    SERVICE_LABEL, SPEC_HASH_LABEL, TaskDiagnostic, TaskState, image_repository,
    valid_logical_name,
};
use piqueld_core::{
    ApplicationId, ObservedApplication, ResourceKind, docker_resource_name,
    docker_resource_readable_prefix,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    future::Future,
    path::Path,
    sync::Arc,
    time::Duration,
};

/// Docker represents health-check and Swarm policy durations in nanoseconds.
const NANOSECONDS_PER_SECOND: i64 = 1_000_000_000;
const NANO_CPUS_PER_MILLICORE: i64 = 1_000_000;
const RESTART_DELAY: i64 = 2 * NANOSECONDS_PER_SECOND;
const UPDATE_MONITOR: i64 = 30 * NANOSECONDS_PER_SECOND;
const HEALTH_RETRIES: i64 = 3;

/// Upper bound for one adapter-level Docker request.
///
/// Bollard only bounds a request up to the response headers, so every adapter
/// call additionally runs under this deadline; an elapsed deadline surfaces as
/// unavailability.
const DOCKER_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Concurrent per-service inspections performed during one observation.
const OBSERVATION_INSPECT_CONCURRENCY: usize = 8;

/// Resolution attempts made before an image tag is declared unstable.
const IMAGE_RESOLVE_ATTEMPTS: usize = 3;
/// Pause between resolution attempts after a suspected concurrent tag flip.
const IMAGE_RESOLVE_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Upper bound for one complete image resolution, including pulls.
///
/// Resolution performs up to [`IMAGE_RESOLVE_ATTEMPTS`] pulls plus digest
/// lookups, so a cold pull of a large image must not share the single-request
/// budget; the reconciler still bounds each prepare phase overall.
pub(crate) const IMAGE_RESOLVE_TIMEOUT: Duration = Duration::from_mins(10);

#[derive(Clone)]
/// A shared connection to the Docker Engine.
pub struct BollardDocker {
    docker: Arc<Docker>,
    socket: Arc<Path>,
}

mod engine;
mod errors;
mod identity;
mod observation;
mod policy;
mod resources;
mod spec;
pub use errors::DockerError;

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
pub trait DockerApi: Send + Sync + 'static {
    /// Ensures that Docker is an active, compatible Swarm manager.
    async fn ensure_swarm(&self, auto_initialize: bool) -> Result<SwarmState, DockerError>;
    /// Pulls an image reference and returns its immutable repository digest.
    async fn resolve_image(&self, reference: &str) -> Result<String, DockerError>;
    /// Reads the resources managed for one application.
    async fn observe(
        &self,
        application: &ApplicationId,
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
pub async fn resolve_image_digest(
    source: &(impl ImageSource + ?Sized),
    reference: &str,
) -> Result<String, DockerError> {
    let repository = image_repository(reference)
        .ok_or(DockerError::ImageResolution("parse image repository"))?;
    for attempt in 0..IMAGE_RESOLVE_ATTEMPTS {
        let before = matching_repo_digests(source, reference, &repository).await?;
        source.pull(reference).await?;
        let after = matching_repo_digests(source, reference, &repository).await?;
        let Some(digest) = after
            .iter()
            .find(|digest| before.is_empty() || before.contains(*digest))
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

/// Bounds one adapter request, surfacing an elapsed deadline as unavailability.
async fn bounded<T>(
    what: &'static str,
    fut: impl Future<Output = Result<T, DockerError>>,
) -> Result<T, DockerError> {
    match tokio::time::timeout(DOCKER_REQUEST_TIMEOUT, fut).await {
        Ok(result) => result,
        Err(_) => Err(DockerError::Unavailable(what)),
    }
}
