use super::{
    Arc, BoundaryError, DockerApi, DockerError, IMAGE_RESOLVE_TIMEOUT, InstanceId,
    NormalizedApplication, Notify, PreparedApplication, ResolutionSet, ResolvedSource,
    RuntimeBoundary, Source, StoredApplication, compile_application,
};
use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt, stream};
use std::time::Duration;

const DOCKER_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Runtime boundary backed by Docker.
pub struct DockerRuntime<D> {
    docker: Arc<D>,
    instance_id: InstanceId,
    wake: Arc<Notify>,
    prepare_timeout: Duration,
}

impl<D> DockerRuntime<D> {
    /// Creates a Docker runtime adapter with the supplied input-resolution budget.
    #[must_use]
    pub fn new(
        docker: Arc<D>,
        instance_id: InstanceId,
        wake: Arc<Notify>,
        prepare_timeout: Duration,
    ) -> Self {
        Self {
            docker,
            instance_id,
            wake,
            prepare_timeout,
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
        tokio::time::timeout(self.prepare_timeout, async {
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
                    // Resolution includes pulls, so it uses the dedicated
                    // image budget rather than the per-request deadline.
                    let digest_reference =
                        tokio::time::timeout(IMAGE_RESOLVE_TIMEOUT, docker.resolve_image(&image))
                            .await
                            .map_err(|_| {
                                BoundaryError::Runtime(DockerError::Unavailable("resolve image"))
                            })??;
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
            let observed =
                tokio::time::timeout(DOCKER_REQUEST_TIMEOUT, self.docker.observe(&application.id))
                    .await
                    .map_err(|_| {
                        BoundaryError::Runtime(DockerError::Unavailable("observe application"))
                    })??;
            Ok(PreparedApplication { resolved, observed })
        })
        .await
        .map_err(|_| BoundaryError::Runtime(DockerError::Unavailable("prepare application")))?
    }

    async fn observe(
        &self,
        application: &StoredApplication,
    ) -> Result<piqueld_core::ObservedApplication, BoundaryError> {
        tokio::time::timeout(
            DOCKER_REQUEST_TIMEOUT,
            self.docker.observe(&application.application.id),
        )
        .await
        .map_err(|_| BoundaryError::Runtime(DockerError::Unavailable("observe application")))?
        .map_err(BoundaryError::from)
    }
}
