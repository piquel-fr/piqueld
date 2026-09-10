//! Resolves accepted manifest inputs during operation execution.

use super::{BoundaryError, RuntimeBoundary};
use crate::{
    docker::{DockerApi, DockerError, IMAGE_RESOLVE_TIMEOUT},
    store::StoredApplication,
};
use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt, stream};
use piqueld_core::{
    InstanceId, NormalizedApplication, ResolutionSet, compile_application, manifest::Source,
    resource::ResolvedSource,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

const DOCKER_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Runtime boundary backed by Docker.
pub struct DockerRuntime<D> {
    docker: Arc<D>,
    instance_id: InstanceId,
    wake: Arc<Notify>,
    prepare_timeout: Duration,
    progress: Option<(Arc<crate::store::SqliteStore>, String)>,
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
            progress: None,
        }
    }
    pub(crate) fn with_progress(
        mut self,
        store: Arc<crate::store::SqliteStore>,
        operation_id: String,
    ) -> Self {
        self.progress = Some((store, operation_id));
        self
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
        reusable: &ResolutionSet,
    ) -> Result<piqueld_core::ResolvedApplication, BoundaryError> {
        tokio::time::timeout(self.prepare_timeout, async {
            let pending = application
                .spec
                .services
                .iter()
                .filter(|service| !reusable.sources.contains_key(&service.name))
                .map(|service| (service.name.clone(), service.source.clone()))
                .collect::<Vec<_>>();
            if let Some((store, id)) = &self.progress
                && !pending.is_empty()
            {
                let names = pending
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                store.progress(id, "resolving_image", Some(&names)).await?;
            }
            let docker = Arc::clone(&self.docker);
            let sources = stream::iter(pending.into_iter().map(move |(name, source)| {
                let docker = Arc::clone(&docker);
                async move {
                    let Source::Image { image } = source;
                    let digest_reference =
                        tokio::time::timeout(IMAGE_RESOLVE_TIMEOUT, docker.resolve_image(&image))
                            .await
                            .unwrap_or_else(|_| Err(DockerError::Unavailable("resolve image")))
                            .map_err(|error| (name.clone(), error))?;
                    Ok::<_, (String, DockerError)>((
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
            .await;
            let sources = match sources {
                Ok(sources) => sources,
                Err((service, error)) => {
                    if let Some((store, id)) = &self.progress {
                        store
                            .progress(id, "resolving_image", Some(&service))
                            .await?;
                    }
                    return Err(BoundaryError::Runtime(error));
                }
            };
            let mut resolutions = reusable.clone();
            resolutions.sources.extend(sources);
            let resolved = compile_application(application, self.instance_id.clone(), &resolutions)
                .map_err(BoundaryError::Compilation)?;
            Ok(resolved)
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
