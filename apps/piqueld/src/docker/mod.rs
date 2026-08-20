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
        ServiceSpecRollbackConfig, ServiceSpecRollbackConfigFailureActionEnum,
        ServiceSpecRollbackConfigOrderEnum, ServiceSpecUpdateConfig,
        ServiceSpecUpdateConfigFailureActionEnum, ServiceSpecUpdateConfigOrderEnum,
        SwarmInitRequest, TaskSpec, TaskSpecContainerSpec, TaskSpecResources,
        TaskSpecRestartPolicy, TaskSpecRestartPolicyConditionEnum, VolumeCreateOptions,
    },
    query_parameters::{
        CreateImageOptionsBuilder, InspectNetworkOptions, InspectServiceOptions,
        ListNetworksOptionsBuilder, ListNodesOptions, ListServicesOptionsBuilder, ListTasksOptions,
        ListVolumesOptionsBuilder,
    },
};
use futures_util::TryStreamExt;
use piqueld_core::manifest::{HealthCheck, ResourceLimits};
use piqueld_core::resource::{
    APPLICATION_LABEL, Convergence, DesiredMount, DesiredNetwork, DesiredService, DesiredVolume,
    INSTANCE_LABEL, MANAGED_LABEL, ObservedNetwork, ObservedService, ObservedTask, ObservedVolume,
    SERVICE_LABEL, SPEC_HASH_LABEL, TaskDiagnostic, TaskState,
};
use piqueld_core::{ApplicationId, ObservedApplication, ResourceKind, docker_resource_name};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
    sync::Arc,
};

/// Docker represents health-check and Swarm policy durations in nanoseconds.
const NANOSECONDS_PER_SECOND: i64 = 1_000_000_000;
const NANOSECONDS_PER_MILLISECOND: i64 = 1_000_000;
const RESTART_DELAY: i64 = 2 * NANOSECONDS_PER_SECOND;
const UPDATE_MONITOR: i64 = 30 * NANOSECONDS_PER_SECOND;
const ROLLBACK_MONITOR: i64 = 5 * NANOSECONDS_PER_SECOND;
const STOP_GRACE_PERIOD: i64 = 10 * NANOSECONDS_PER_SECOND;
const HEALTH_RETRIES: i64 = 3;

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
