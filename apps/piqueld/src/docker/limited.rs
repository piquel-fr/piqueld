//! Process-wide concurrency limits shared by reconciliation and API previews.
//!
//! Applications reconcile concurrently, so per-application limits alone would
//! allow an unbounded number of Docker pulls, builds, and observations. This
//! adapter puts those limits around the shared Docker implementation
//! (including test fakes). Mutations pass through unchanged: the controller
//! serializes them. Cancelling a request drops its permit, allowing the next
//! waiter to proceed.
use super::build_queue::BuildQueue;
use super::{DockerApi, DockerError, DockerTimeout, SwarmState};
use async_trait::async_trait;
use piqueld_core::{
    DesiredNetwork, DesiredService, DesiredVolume, EnvironmentId, ObservedApplication,
};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::Semaphore;

/// A [`DockerApi`] wrapper that bounds process-wide concurrent Docker work.
pub(crate) struct LimitedDocker<D> {
    /// The wrapped implementation that performs the actual requests.
    inner: Arc<D>,
    /// Concurrent image resolutions (pulls).
    images: Semaphore,
    /// One image build at a time, environments before previews.
    builds: BuildQueue,
    /// Concurrent observations and log reads.
    observations: Semaphore,
}
impl<D> LimitedDocker<D> {
    /// Wraps `inner` with 2 image and 8 observation permits, and a queue
    /// running one build at a time.
    pub(crate) fn new(inner: Arc<D>) -> Self {
        Self {
            inner,
            images: Semaphore::new(2),
            builds: BuildQueue::default(),
            observations: Semaphore::new(8),
        }
    }
}
#[async_trait]
impl<D: DockerApi> DockerApi for LimitedDocker<D> {
    async fn application_logs(
        &self,
        instance: &piqueld_core::InstanceId,
        application: &EnvironmentId,
        service: Option<&str>,
        tail: u16,
        since: u32,
        stream: Option<piqueld_core::api::LogStream>,
    ) -> Result<piqueld_core::api::ApplicationLogs, DockerError> {
        let _permit = self
            .observations
            .acquire()
            .await
            .expect("semaphore never closed");
        self.inner
            .application_logs(instance, application, service, tail, since, stream)
            .await
    }

    async fn ping(&self) -> Result<(), DockerError> {
        self.inner.ping().await
    }

    async fn create_exec(
        &self,
        instance: &piqueld_core::InstanceId,
        environment: &EnvironmentId,
        request: &piqueld_core::exec::ExecRequest,
    ) -> Result<Option<super::Exec>, DockerError> {
        self.inner.create_exec(instance, environment, request).await
    }

    async fn run_exec(&self, exec: &super::Exec, io: super::ExecIo) -> Result<i64, DockerError> {
        self.inner.run_exec(exec, io).await
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
    /// Waiting for an image permit counts against the resolution budget.
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
        build: &super::ImageBuild<'_>,
        log: Option<&crate::build::BuildLog>,
    ) -> Result<piqueld_core::resource::Sha256Digest, DockerError> {
        let _turn = self.builds.turn(build.priority).await;
        self.inner.build_image_recorded(build, log).await
    }
    async fn build_image(
        &self,
        build: &super::ImageBuild<'_>,
    ) -> Result<piqueld_core::resource::Sha256Digest, DockerError> {
        self.build_image_recorded(build, None).await
    }
    /// Waiting for an observation permit counts against the request budget.
    async fn observe(&self, id: &EnvironmentId) -> Result<ObservedApplication, DockerError> {
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
    /// Listing images counts as an observation.
    async fn images(&self) -> Result<Vec<super::LocalImage>, DockerError> {
        let _permit = self
            .observations
            .acquire()
            .await
            .expect("semaphore is never closed");
        self.inner.images().await
    }
    async fn remove_image(
        &self,
        instance: &piqueld_core::InstanceId,
        id: &piqueld_core::Sha256Digest,
    ) -> Result<(), DockerError> {
        self.inner.remove_image(instance, id).await
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
    async fn start_job(&self, job: &piqueld_core::DesiredJobRun) -> Result<(), DockerError> {
        self.inner.start_job(job).await
    }
    async fn job_status(
        &self,
        job: &piqueld_core::DesiredJobRun,
    ) -> Result<super::JobStatus, DockerError> {
        let _permit = self
            .observations
            .acquire()
            .await
            .expect("semaphore never closed");
        self.inner.job_status(job).await
    }
    async fn job_output(
        &self,
        job: &piqueld_core::DesiredJobRun,
    ) -> Result<super::JobOutput, DockerError> {
        let _permit = self
            .observations
            .acquire()
            .await
            .expect("semaphore never closed");
        self.inner.job_output(job).await
    }
    async fn remove_jobs(
        &self,
        ownership: &BTreeMap<String, String>,
        runs: super::JobRuns<'_>,
    ) -> Result<(), DockerError> {
        self.inner.remove_jobs(ownership, runs).await
    }
    async fn remove_network(
        &self,
        name: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        self.inner.remove_network(name, ownership).await
    }
    async fn remove_volume(
        &self,
        name: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        self.inner.remove_volume(name, ownership).await
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
            .observe(&EnvironmentId::parse("app-queued").unwrap())
            .await
            .unwrap_err();
        assert_eq!(started.elapsed(), DockerTimeout::Request.duration());
        assert!(error.source().unwrap().is::<tokio::time::error::Elapsed>());
        drop(permits);
        assert_eq!(docker.observations.available_permits(), 8);
    }
}
