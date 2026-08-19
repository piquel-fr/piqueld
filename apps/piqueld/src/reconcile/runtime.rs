use super::{
    Arc, BoundaryError, DockerApi, InstanceId, NormalizedApplication, Notify, PreparedApplication,
    ResolutionSet, ResolvedSource, RuntimeBoundary, Source, StoredApplication, compile_application,
};
use async_trait::async_trait;

/// Runtime boundary backed by Docker.
pub struct DockerRuntime<D> {
    docker: Arc<D>,
    instance_id: InstanceId,
    wake: Arc<Notify>,
}

impl<D> DockerRuntime<D> {
    /// Creates a Docker runtime adapter.
    #[must_use]
    pub fn new(docker: Arc<D>, instance_id: InstanceId, wake: Arc<Notify>) -> Self {
        Self {
            docker,
            instance_id,
            wake,
        }
    }
}

#[async_trait]
impl<D: DockerApi> RuntimeBoundary for DockerRuntime<D> {
    fn trigger_reconciliation(&self) {
        self.wake.notify_one();
    }

    async fn prepare(
        &self,
        application: &NormalizedApplication,
    ) -> Result<PreparedApplication, BoundaryError> {
        let mut resolutions = ResolutionSet::default();
        for service in &application.spec.services {
            let Source::Image { image } = &service.source;
            let digest_reference = self.docker.resolve_image(image).await?;
            resolutions.sources.insert(
                service.name.clone(),
                ResolvedSource::Image {
                    requested: image.clone(),
                    digest_reference,
                },
            );
        }
        let resolved = compile_application(application, self.instance_id.clone(), &resolutions)
            .map_err(BoundaryError::Compilation)?;
        let observed = self.docker.observe(&application.id).await?;
        Ok(PreparedApplication { resolved, observed })
    }

    async fn observe(
        &self,
        application: &StoredApplication,
    ) -> Result<piqueld_core::ObservedApplication, BoundaryError> {
        Ok(self.docker.observe(&application.application.id).await?)
    }
}
