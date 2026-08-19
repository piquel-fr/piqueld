use super::{
    Arc, BoundaryError, DockerApi, DockerError, InstanceId, NormalizedApplication, Notify,
    PreparedApplication, ResolutionSet, ResolvedSource, RuntimeBoundary, Source, StoredApplication,
    compile_application,
};
use crate::api::PreparedBuild;
use crate::build::SourceBuilder;
use crate::secrets::{SecretError, SecretService};
use async_trait::async_trait;
use std::time::Duration;

const PREPARE_TIMEOUT: Duration = Duration::from_mins(5);
const DOCKER_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Runtime boundary backed by Docker.
pub struct DockerRuntime<D> {
    docker: Arc<D>,
    instance_id: InstanceId,
    wake: Arc<Notify>,
    secret_service: Option<Arc<SecretService>>,
    source_builder: Option<Arc<dyn SourceBuilder>>,
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
            source_builder: None,
        }
    }

    /// Enables encrypted logical-secret resolution during preparation.
    #[must_use]
    pub fn with_secret_service(mut self, service: Arc<SecretService>) -> Self {
        self.secret_service = Some(service);
        self
    }

    /// Enables Git/Dockerfile source preparation.
    #[must_use]
    pub fn with_source_builder(mut self, builder: Arc<dyn SourceBuilder>) -> Self {
        self.source_builder = Some(builder);
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
            let mut resolutions = ResolutionSet::default();
            let mut builds = Vec::new();
            for service in &application.spec.services {
                let resolved = match &service.source {
                    Source::Image { image } => {
                        let digest_reference = tokio::time::timeout(
                            DOCKER_REQUEST_TIMEOUT,
                            self.docker.resolve_image(image),
                        )
                        .await
                        .map_err(|_| {
                            BoundaryError::Runtime(DockerError::Unavailable("resolve image"))
                        })??;
                        ResolvedSource::Image {
                            requested: image.clone(),
                            digest_reference,
                        }
                    }
                    Source::Git {
                        repository,
                        reference,
                        context,
                        dockerfile,
                    } => {
                        let builder = self
                            .source_builder
                            .as_ref()
                            .ok_or(crate::build::BuildError::Git)?;
                        let built = builder
                            .build_source(
                                application.id.as_str(),
                                &service.name,
                                repository,
                                reference,
                                context,
                                dockerfile,
                            )
                            .await?;
                        builds.push(PreparedBuild {
                            service_name: service.name.clone(),
                            source_commit: built.commit.clone(),
                            image_reference: built.registry_reference.clone(),
                            image_digest: built.digest_reference.clone(),
                            build_key: built.build_key.clone(),
                            context_hash: built.context_hash.clone(),
                            logs: built.logs.clone(),
                        });
                        ResolvedSource::Git {
                            repository: repository.clone(),
                            requested_reference: reference.clone(),
                            commit: built.commit,
                            context: context.clone(),
                            dockerfile: dockerfile.clone(),
                            registry_reference: built.registry_reference,
                            digest_reference: built.digest_reference,
                        }
                    }
                };
                resolutions.sources.insert(service.name.clone(), resolved);
            }

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
            Ok(PreparedApplication {
                resolved,
                observed,
                builds,
            })
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
