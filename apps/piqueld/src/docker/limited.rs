//! Process-wide concurrency limits shared by reconciliation and API previews.
//!
//! Applications reconcile concurrently, so per-application limits alone would
//! allow an unbounded number of Docker pulls and observations. This adapter puts
//! those two limits around the shared Docker implementation (including test
//! fakes). Mutations pass through unchanged: the controller serializes them.
//! Cancelling a request drops its permit, allowing the next waiter to proceed.
use super::{DockerApi, DockerError, SwarmState};
use async_trait::async_trait;
use piqueld_core::{
    ApplicationId, DesiredNetwork, DesiredService, DesiredVolume, ObservedApplication,
};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::Semaphore;

pub(crate) struct LimitedDocker<D> {
    inner: Arc<D>,
    images: Semaphore,
    builds: Semaphore,
    observations: Semaphore,
}
impl<D> LimitedDocker<D> {
    pub(crate) fn new(inner: Arc<D>) -> Self {
        Self {
            inner,
            images: Semaphore::new(2),
            builds: Semaphore::new(1),
            observations: Semaphore::new(8),
        }
    }
}
#[async_trait]
impl<D: DockerApi> DockerApi for LimitedDocker<D> {
    async fn ensure_swarm(&self, auto: bool) -> Result<SwarmState, DockerError> {
        self.inner.ensure_swarm(auto).await
    }
    async fn resolve_image(&self, reference: &str) -> Result<String, DockerError> {
        let _permit = self
            .images
            .acquire()
            .await
            .expect("semaphore is never closed");
        self.inner.resolve_image(reference).await
    }
    async fn build_git(
        &self,
        repository: &piqueld_core::manifest::GitRepository,
        build: &piqueld_core::manifest::Build,
    ) -> Result<(String, piqueld_core::resource::Sha256Digest), DockerError> {
        let _permit = self
            .builds
            .acquire()
            .await
            .expect("semaphore is never closed");
        self.inner.build_git(repository, build).await
    }
    async fn observe(&self, id: &ApplicationId) -> Result<ObservedApplication, DockerError> {
        let _permit = self
            .observations
            .acquire()
            .await
            .expect("semaphore is never closed");
        self.inner.observe(id).await
    }
    async fn ensure_network(&self, value: &DesiredNetwork) -> Result<(), DockerError> {
        self.inner.ensure_network(value).await
    }
    async fn ensure_volume(&self, value: &DesiredVolume) -> Result<(), DockerError> {
        self.inner.ensure_volume(value).await
    }
    async fn ensure_service(&self, value: &DesiredService) -> Result<(), DockerError> {
        self.inner.ensure_service(value).await
    }
    async fn remove_service(
        &self,
        name: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        self.inner.remove_service(name, ownership).await
    }
    async fn remove_network(
        &self,
        name: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        self.inner.remove_network(name, ownership).await
    }
}
