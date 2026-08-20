use super::{
    Arc, BoundaryError, DockerApi, InstanceId, NormalizedApplication, Notify, PreparedApplication,
    ResolutionSet, ResolvedSource, RuntimeBoundary, Source, StoredApplication, compile_application,
};
use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt, stream};

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
        let docker = Arc::clone(&self.docker);
        let jobs = application
            .spec
            .services
            .iter()
            .map(|service| (service.name.clone(), service.source.clone()))
            .collect::<Vec<_>>();
        let sources = stream::iter(jobs.into_iter().map(|(name, source)| {
            let docker = Arc::clone(&docker);
            async move {
                let Source::Image { image } = source;
                let digest_reference = docker.resolve_image(&image).await?;
                Ok::<_, BoundaryError>((
                    name,
                    ResolvedSource::Image {
                        requested: image,
                        digest_reference,
                    },
                ))
            }
        }))
        .buffer_unordered(4)
        .try_collect::<Vec<_>>()
        .await?;
        let resolutions = ResolutionSet {
            sources: sources.into_iter().collect(),
        };
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
