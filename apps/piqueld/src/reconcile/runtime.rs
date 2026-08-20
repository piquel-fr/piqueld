use super::{
    Arc, BoundaryError, DockerApi, DockerError, InstanceId, NormalizedApplication, Notify,
    PreparedApplication, ResolutionSet, ResolvedSource, RuntimeBoundary, Source, StoredApplication,
    compile_application,
};
use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt, stream};
use std::time::Duration;

const PREPARE_TIMEOUT: Duration = Duration::from_mins(5);

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
    /// Wakes one waiting reconciliation task.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// runtime.trigger_reconciliation();
    /// ```
    fn trigger_reconciliation(&self) {
        self.wake.notify_one();
    }

    /// Prepares an application for execution by resolving its image sources, compiling it, and retrieving its observed state.
    ///
    /// # Errors
    ///
    /// Returns a boundary error if image resolution, application compilation, Docker observation, or the preparation timeout fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// let prepared = runtime.prepare(&application).await?;
    /// # Ok::<(), BoundaryError>(())
    /// ```
    async fn prepare(
        &self,
        application: &NormalizedApplication,
    ) -> Result<PreparedApplication, BoundaryError> {
        tokio::time::timeout(PREPARE_TIMEOUT, async {
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
        })
        .await
        .map_err(|_| BoundaryError::Runtime(DockerError::Unavailable("prepare application")))?
    }

    /// Retrieves the current observed state of a stored application from Docker.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// let observed = runtime.observe(&application).await?;
    /// # Ok::<(), BoundaryError>(())
    /// ```
    ///
    /// # Returns
    ///
    /// The application's current observed state.
    async fn observe(
        &self,
        application: &StoredApplication,
    ) -> Result<piqueld_core::ObservedApplication, BoundaryError> {
        Ok(self.docker.observe(&application.application.id).await?)
    }
}
