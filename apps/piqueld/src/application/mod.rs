//! Runtime preparation and observation boundaries.

mod runtime;
pub use runtime::ApplicationRuntime;

use crate::reconcile::ImagesInUse;
use crate::{
    docker::{
        ContainerLogs, ContainerStarts, ContainerUsage, DockerError, Exec, ExecIo, ManagedContainer,
    },
    store::{StoreError, StoredEnvironment},
};
use async_trait::async_trait;
use piqueld_core::{
    CompileError, EnvironmentId, NormalizedApplication, ObservedApplication, ServiceName,
    resource::{ResolvedApplication, ResolvedSource},
};
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
/// Errors crossing the runtime boundary.
pub enum BoundaryError {
    /// A Docker runtime request failed.
    #[error("runtime request failed")]
    Runtime(#[from] DockerError),
    /// Git checkout or image building failed, retaining internal diagnostics.
    #[error("Git source build failed")]
    GitBuild(#[source] anyhow::Error),
    /// Progress persistence failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Resolved inputs could not be compiled into desired runtime resources.
    #[error("application compilation failed")]
    Compilation(Vec<CompileError>),
    /// An image a release runs is gone: a build, or a registry image that
    /// couldn't be pulled again by digest.
    #[error(transparent)]
    ImageUnavailable(Box<crate::operations::OperationError>),
}

/// Resolves accepted intent during execution and observes runtime for API reads.
///
/// Methods have no default implementations: every adapter states its own
/// behavior, so a new method cannot be silently missing from one of them.
#[async_trait]
pub trait RuntimeBoundary: Send + Sync + 'static {
    /// Reads recent workload logs from Docker.
    ///
    /// `service` narrows to one service, `tail` caps the returned lines, `since` is a
    /// look-back window in seconds, and `stream` optionally filters stdout or stderr.
    async fn logs(
        &self,
        id: &EnvironmentId,
        service: Option<&str>,
        tail: u16,
        since: u32,
        stream: Option<piqueld_core::api::LogStream>,
    ) -> Result<piqueld_core::api::ApplicationLogs, BoundaryError>;

    /// Returns separate Engine and Swarm probe results.
    async fn readiness(&self) -> (bool, bool);

    /// Waits for the Docker mutations of operations and repair in progress,
    /// and keeps new ones from starting until the guard drops. Secret
    /// deletion holds it from checking references to finishing.
    async fn lock_mutations(&self) -> tokio::sync::OwnedMutexGuard<()>;
    /// Removes the supplied versions after logical-reference checks succeed.
    async fn remove_secrets(
        &self,
        environment: &EnvironmentId,
        names: &[String],
    ) -> Result<(), BoundaryError>;
    /// Creates a command in one running task of the environment's service.
    /// Returns `None` when the service has no running task.
    async fn create_exec(
        &self,
        environment: &EnvironmentId,
        request: &piqueld_core::exec::ExecRequest,
    ) -> Result<Option<Exec>, BoundaryError>;
    /// Streams a created command until it exits and returns its exit code.
    async fn run_exec(&self, exec: &Exec, io: ExecIo) -> Result<i64, BoundaryError>;
    /// Wakes the reconciler after a mutation requests an immediate scan.
    fn trigger_reconciliation(&self);
    /// Resolves all mutable inputs into a complete immutable target for an environment.
    async fn prepare(
        &self,
        environment: &EnvironmentId,
        application: &NormalizedApplication,
        resolutions: &piqueld_core::ResolutionSet,
    ) -> Result<ResolvedApplication, BoundaryError>;
    /// Lists the images present in the local engine.
    async fn local_images(&self) -> Result<piqueld_core::LocalImages, BoundaryError>;
    /// Keeps image cleanup from removing anything until the returned guard
    /// drops, and makes sure every image of `sources` is present, pulling
    /// registry images again by digest (see `ImagesInUse::ensure`). Promoting
    /// a release holds the guard until it saved the target that runs them.
    async fn reuse_images(
        &self,
        sources: &BTreeMap<ServiceName, ResolvedSource>,
    ) -> Result<ImagesInUse, BoundaryError>;
    /// Checks Docker availability without preparing images or changing runtime resources.
    async fn check_available(&self) -> Result<(), BoundaryError>;
    /// Captures current runtime state for a stored environment.
    async fn observe(
        &self,
        application: &StoredEnvironment,
    ) -> Result<ObservedApplication, BoundaryError>;
    /// Lists every container the installation runs, stopped ones included,
    /// for container telemetry.
    async fn containers(&self) -> Result<Vec<ManagedContainer>, BoundaryError>;
    /// Reads a running container's resource usage once; `None` once it is
    /// gone.
    async fn container_usage(&self, id: &str) -> Result<Option<ContainerUsage>, BoundaryError>;
    /// Follows a container's output from `since`, in Unix seconds, until it
    /// stops.
    fn container_logs(&self, id: &str, since: i32) -> ContainerLogs;
    /// Yields each container start from `since`, in Unix seconds.
    fn container_starts(&self, since: i64) -> ContainerStarts;
}
