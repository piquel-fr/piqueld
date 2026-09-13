//! Process-wide concurrency limits shared by reconciliation and API previews.
//!
//! Applications reconcile concurrently, so per-application limits alone would
//! allow an unbounded number of Docker pulls and observations. This adapter puts
//! those two limits around the shared Docker implementation (including test
//! fakes). Mutations pass through unchanged: the controller serializes them.
//! Cancelling a request drops its permit, allowing the next waiter to proceed.
use super::{DockerApi, DockerError, DockerTimeout, SwarmState};
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
    async fn application_logs(
        &self,
        instance: &piqueld_core::InstanceId,
        application: &ApplicationId,
        service: Option<&str>,
        tail: u16,
        since: u32,
    ) -> Result<piqueld_core::api::ApplicationLogs, DockerError> {
        let _permit = self
            .observations
            .acquire()
            .await
            .expect("semaphore never closed");
        self.inner
            .application_logs(instance, application, service, tail, since)
            .await
    }

    async fn ping(&self) -> Result<(), DockerError> {
        self.inner.ping().await
    }

    async fn ensure_secret(
        &self,
        name: &str,
        value: &[u8],
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        self.inner.ensure_secret(name, value, ownership).await
    }
    async fn remove_secrets(
        &self,
        names: &[String],
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        self.inner.remove_secrets(names, ownership).await
    }
    async fn ensure_swarm(&self, auto: bool) -> Result<SwarmState, DockerError> {
        self.inner.ensure_swarm(auto).await
    }
    async fn resolve_image(&self, reference: &str) -> Result<String, DockerError> {
        DockerTimeout::ImageResolution
            .run("resolve image", async {
                let _permit = self
                    .images
                    .acquire()
                    .await
                    .expect("semaphore is never closed");
                self.inner.resolve_image(reference).await
            })
            .await
    }
    async fn build_image_recorded(
        &self,
        dockerfile: &std::path::Path,
        context: &std::path::Path,
        log: Option<&crate::build::BuildLog>,
    ) -> Result<piqueld_core::resource::Sha256Digest, DockerError> {
        let _permit = self
            .builds
            .acquire()
            .await
            .map_err(|_| DockerError::Unavailable("build concurrency gate"))?;
        self.inner
            .build_image_recorded(dockerfile, context, log)
            .await
    }
    async fn build_image(
        &self,
        dockerfile: &std::path::Path,
        context: &std::path::Path,
    ) -> Result<piqueld_core::resource::Sha256Digest, DockerError> {
        self.build_image_recorded(dockerfile, context, None).await
    }
    async fn observe(&self, id: &ApplicationId) -> Result<ObservedApplication, DockerError> {
        DockerTimeout::Request
            .run("observe application", async {
                let _permit = self
                    .observations
                    .acquire()
                    .await
                    .expect("semaphore is never closed");
                self.inner.observe(id).await
            })
            .await
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn observation_budget_includes_waiting_for_a_permit() {
        use std::error::Error;
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("docker.sock");
        let _listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let docker = LimitedDocker::new(Arc::new(
            super::super::BollardDocker::connect(&socket).unwrap(),
        ));
        let permits = docker.observations.acquire_many(8).await.unwrap();
        let started = tokio::time::Instant::now();
        let error = docker
            .observe(&ApplicationId::parse("app-queued").unwrap())
            .await
            .unwrap_err();
        assert_eq!(started.elapsed(), DockerTimeout::Request.duration());
        assert!(error.source().unwrap().is::<tokio::time::error::Elapsed>());
        drop(permits);
        assert_eq!(docker.observations.available_permits(), 8);
    }
}
