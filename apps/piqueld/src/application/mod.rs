//! Runtime preparation and observation boundaries.

mod runtime;
pub use runtime::ApplicationRuntime;

use crate::{
    docker::DockerError,
    store::{StoreError, StoredApplication},
};
use async_trait::async_trait;
use piqueld_core::{
    ApplicationId, CompileError, NormalizedApplication, ObservedApplication,
    resource::ResolvedApplication,
};

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
}

/// Resolves accepted intent during execution and observes runtime for API reads.
#[async_trait]
pub trait RuntimeBoundary: Send + Sync + 'static {
    /// Reads recent workload logs from Docker.
    async fn logs(
        &self,
        id: &ApplicationId,
        service: Option<&str>,
        tail: u16,
        since: u32,
        stream: Option<piqueld_core::api::LogStream>,
    ) -> Result<piqueld_core::api::ApplicationLogs, BoundaryError>;

    /// Returns separate Engine and Swarm probe results. Unknown adapters fail closed.
    async fn readiness(&self) -> (bool, bool) {
        (false, false)
    }

    /// Wakes the reconciler after a mutation requests an immediate scan.
    fn trigger_reconciliation(&self) {}
    /// Resolves all mutable inputs into a complete immutable target.
    async fn prepare(
        &self,
        application: &NormalizedApplication,
        resolutions: &piqueld_core::ResolutionSet,
    ) -> Result<ResolvedApplication, BoundaryError>;
    /// Checks Docker availability without preparing images or changing runtime resources.
    async fn check_available(&self) -> Result<(), BoundaryError>;
    /// Captures current runtime state for a stored application.
    async fn observe(
        &self,
        application: &StoredApplication,
    ) -> Result<ObservedApplication, BoundaryError>;
}
