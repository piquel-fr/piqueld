use super::{
    Arc, BoundaryError, DockerApi, DockerError, InstanceId, NormalizedApplication, Notify,
    PreparedApplication, ResolutionSet, ResolvedSource, RuntimeBoundary, Source, StoredApplication,
    compile_application,
};
use crate::secrets::{SecretError, SecretService};
use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt, stream};
use std::time::Duration;

const PREPARE_TIMEOUT: Duration = Duration::from_mins(5);
const DOCKER_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Runtime boundary backed by Docker.
pub struct DockerRuntime<D> {
    docker: Arc<D>,
    instance_id: InstanceId,
    wake: Arc<Notify>,
    secret_service: Option<Arc<SecretService>>,
}

impl<D> DockerRuntime<D> {
    /// Creates a Docker runtime adapter.
    #[must_use]
    pub fn new(docker: Arc<D>, instance_id: InstanceId, wake: Arc<Notify>) -> Self {
        Self {
            docker,
            instance_id,
            wake,
            secret_service: None,
        }
    }

    /// Enables encrypted logical-secret resolution during preparation.
    #[must_use]
    pub fn with_secret_service(mut self, service: Arc<SecretService>) -> Self {
        self.secret_service = Some(service);
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
                    let digest_reference =
                        tokio::time::timeout(DOCKER_REQUEST_TIMEOUT, docker.resolve_image(&image))
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
            let mut resolutions = ResolutionSet {
                sources: sources.into_iter().collect(),
                ..ResolutionSet::default()
            };
            let names = application
                .logical_secret_references()
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if !names.is_empty() {
                let service = self
                    .secret_service
                    .as_ref()
                    .ok_or(BoundaryError::Secrets(SecretError::KeyUnavailable))?;
                for generation in service.current_generations(names, &application.id).await? {
                    resolutions
                        .secrets
                        .insert(generation.logical_name.clone(), generation);
                }
            }
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
        Ok(self.docker.observe(&application.application.id).await?)
    }
}
