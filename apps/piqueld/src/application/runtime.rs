//! Resolves accepted manifest inputs during operation execution.

use super::{BoundaryError, RuntimeBoundary};
use crate::{
    docker::{DockerApi, DockerError, DockerTimeout},
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

/// Runtime boundary backed by Docker.
pub struct DockerRuntime<D> {
    docker: Arc<D>,
    instance_id: InstanceId,
    wake: Arc<Notify>,
    prepare_timeout: Duration,
    // Set only for execution, never API previews. Records the source-preparation
    // phase and service names in the existing operation row so polling/events
    // can explain a slow or failed pull; it is not an execution journal.
    progress: Option<(Arc<crate::store::Store>, String)>,
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
    /// Associates source preparation with the operation whose status is reported.
    pub(crate) fn with_progress(
        mut self,
        store: Arc<crate::store::Store>,
        operation_id: String,
    ) -> Self {
        self.progress = Some((store, operation_id));
        self
    }
}

#[async_trait]
impl<D: DockerApi> RuntimeBoundary for DockerRuntime<D> {
    async fn logs(
        &self,
        id: &piqueld_core::ApplicationId,
        service: Option<&str>,
        tail: u16,
        since: u32,
    ) -> Result<piqueld_core::api::ApplicationLogs, BoundaryError> {
        tokio::time::timeout(
            Duration::from_secs(10),
            self.docker
                .application_logs(&self.instance_id, id, service, tail, since),
        )
        .await
        .map_err(|_| DockerError::Unavailable("read application logs"))?
        .map_err(BoundaryError::from)
    }

    async fn readiness(&self) -> (bool, bool) {
        let docker = tokio::time::timeout(Duration::from_secs(2), self.docker.ping())
            .await
            .is_ok_and(|r| r.is_ok());
        let swarm = docker
            && tokio::time::timeout(Duration::from_secs(3), self.docker.ensure_swarm(false))
                .await
                .is_ok_and(|r| r.is_ok());
        (docker, swarm)
    }

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
                .spec()
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
                store
                    .progress(id, "preparing_sources", Some(&names))
                    .await?;
            }
            let docker = Arc::clone(&self.docker);
            let sources = stream::iter(pending.into_iter().map(move |(name, source)| {
                let docker = Arc::clone(&docker);
                async move {
                    let resolved = match &source {
                        Source::Image { image } => {
                            let digest_reference = DockerTimeout::ImageResolution
                                .run("resolve image", docker.resolve_image(image))
                                .await
                                .map_err(|error| {
                                    (
                                        name.clone(),
                                        "resolving_image",
                                        BoundaryError::Runtime(error),
                                    )
                                })?;
                            ResolvedSource::parse_image(image.clone(), digest_reference).map_err(
                                |source| {
                                    (
                                        name.clone(),
                                        "resolving_image",
                                        BoundaryError::Runtime(DockerError::RequestSource {
                                            operation: "validate resolved image",
                                            source: Box::new(source),
                                        }),
                                    )
                                },
                            )?
                        }
                        Source::Git { repository, build } => {
                            let (commit, image_id) = self
                                .prepare_git(
                                    application,
                                    name.as_str(),
                                    &source,
                                    repository,
                                    build,
                                    docker.as_ref(),
                                )
                                .await
                                .map_err(|error| {
                                    (name.clone(), "building_git", BoundaryError::GitBuild(error))
                                })?;
                            ResolvedSource::Git {
                                requested: source,
                                commit,
                                image_id,
                            }
                        }
                    };
                    Ok::<_, (piqueld_core::ServiceName, &'static str, BoundaryError)>((
                        name, resolved,
                    ))
                }
            }))
            .buffer_unordered(4)
            .try_collect::<Vec<_>>()
            .await;
            let sources = match sources {
                Ok(sources) => sources,
                Err((service, phase, error)) => {
                    if let Some((store, id)) = &self.progress {
                        store.progress(id, phase, Some(service.as_str())).await?;
                    }
                    return Err(error);
                }
            };
            let mut resolutions = reusable.clone();
            resolutions.sources.extend(sources);
            let resolved = compile_application(application, self.instance_id.clone(), &resolutions)
                .map_err(BoundaryError::Compilation)?;
            Ok(resolved)
        })
        .await
        .map_err(|source| {
            BoundaryError::Runtime(DockerError::unavailable("prepare application", source))
        })?
    }

    async fn check_available(&self) -> Result<(), BoundaryError> {
        DockerTimeout::Request
            .run("check Docker availability", self.docker.ensure_swarm(false))
            .await?;
        Ok(())
    }

    async fn observe(
        &self,
        application: &StoredApplication,
    ) -> Result<piqueld_core::ObservedApplication, BoundaryError> {
        DockerTimeout::Request
            .run(
                "observe application",
                self.docker.observe(application.application.id()),
            )
            .await
            .map_err(BoundaryError::from)
    }
}

impl<D: DockerApi> DockerRuntime<D> {
    async fn prepare_git(
        &self,
        application: &NormalizedApplication,
        service: &str,
        source: &Source,
        repository: &piqueld_core::manifest::GitRepository,
        build: &piqueld_core::manifest::Build,
        docker: &D,
    ) -> anyhow::Result<(String, piqueld_core::resource::Sha256Digest)> {
        let Some((store, operation)) = &self.progress else {
            return crate::git::Checkout::prepare(repository, build, docker).await;
        };
        let attempt = crate::build::BuildAttempt::start(
            Arc::clone(store),
            application.id(),
            operation,
            service,
            source,
        )
        .await?;
        let result =
            crate::git::Checkout::prepare_recorded(repository, build, docker, Some(&attempt.log))
                .await;
        match &result {
            Ok((_, image)) => {
                attempt
                    .finish(
                        piqueld_core::api::BuildState::Succeeded,
                        Some(image.as_str()),
                    )
                    .await?;
            }
            Err(error) => {
                attempt
                    .log
                    .append(format!("\nBuild failed: {error}\n").as_bytes())
                    .await?;
                attempt
                    .finish(piqueld_core::api::BuildState::Failed, None)
                    .await?;
            }
        }
        result
    }
}
