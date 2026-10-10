//! Resolves accepted manifest inputs during operation execution.

use super::{BoundaryError, RuntimeBoundary};
use crate::{
    docker::{BuildPriority, DockerApi, DockerError, DockerTimeout},
    store::StoredEnvironment,
};
use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt, stream};
use piqueld_core::{
    EnvironmentId, EnvironmentKind, InstanceId, NormalizedApplication, ResolutionSet,
    compile_application,
    manifest::{SourceRepository, ValidatedSource as Source},
    resource::ResolvedSource,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

/// Application runtime orchestration backed by Docker.
pub struct ApplicationRuntime<D> {
    docker: Arc<D>,
    /// Instance whose ownership labels scope every Docker lookup and mutation.
    instance_id: InstanceId,
    /// Shared with the reconciler loop so mutations can request an immediate scan.
    wake: Arc<Notify>,
    /// Upper bound for resolving every source and compiling one application.
    prepare_timeout: Duration,
    // Set only for execution, never API previews. Records the source-preparation
    // phase and service names, and journals each source-preparation action.
    progress: Option<(Arc<crate::store::Store>, String)>,
    /// The build queue its Git sources wait in.
    priority: BuildPriority,
}

impl<D> ApplicationRuntime<D> {
    /// Creates application runtime orchestration with the supplied preparation budget.
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
            priority: BuildPriority::Environment,
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

    /// Queues Git source builds as `kind`'s, so environments build before
    /// previews.
    #[must_use]
    pub(crate) fn with_build_priority(mut self, kind: &EnvironmentKind) -> Self {
        self.priority = kind.into();
        self
    }
}

#[async_trait]
impl<D: DockerApi> RuntimeBoundary for ApplicationRuntime<D> {
    async fn logs(
        &self,
        id: &piqueld_core::EnvironmentId,
        service: Option<&str>,
        tail: u16,
        since: u32,
        stream: Option<piqueld_core::api::LogStream>,
    ) -> Result<piqueld_core::api::ApplicationLogs, BoundaryError> {
        // Log reads serve interactive API requests, so they get a short fixed budget.
        tokio::time::timeout(
            Duration::from_secs(10),
            self.docker
                .application_logs(&self.instance_id, id, service, tail, since, stream),
        )
        .await
        .map_err(|_| DockerError::Unavailable("read application logs"))?
        .map_err(BoundaryError::from)
    }

    /// Pings the Engine first and only probes Swarm when the Engine answered.
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

    /// Removes secrets only when they carry this instance's and application's
    /// ownership labels, so foreign Docker secrets are never deleted.
    async fn remove_secrets(
        &self,
        application: &piqueld_core::EnvironmentId,
        names: &[String],
    ) -> Result<(), BoundaryError> {
        let ownership = std::collections::BTreeMap::from([
            (piqueld_core::resource::MANAGED_LABEL.into(), "true".into()),
            (
                piqueld_core::resource::APPLICATION_LABEL.into(),
                application.to_string(),
            ),
            (
                piqueld_core::resource::INSTANCE_LABEL.into(),
                self.instance_id.to_string(),
            ),
        ]);
        DockerTimeout::Request
            .run(
                "remove secrets",
                self.docker.remove_secrets(names, &ownership),
            )
            .await?;
        Ok(())
    }
    async fn create_exec(
        &self,
        environment: &piqueld_core::EnvironmentId,
        request: &piqueld_core::exec::ExecRequest,
    ) -> Result<Option<crate::docker::Exec>, BoundaryError> {
        Ok(self
            .docker
            .create_exec(&self.instance_id, environment, request)
            .await?)
    }
    async fn run_exec(
        &self,
        exec: &crate::docker::Exec,
        io: crate::docker::ExecIo,
    ) -> Result<i64, BoundaryError> {
        Ok(self.docker.run_exec(exec, io).await?)
    }
    fn trigger_reconciliation(&self) {
        self.wake.notify_one();
    }

    /// Resolves every service source not already present in `reusable`, then
    /// compiles the application for `environment` against the merged resolutions.
    ///
    /// 1. Collects services lacking a reusable resolution and, during execution,
    ///    records the `preparing_sources` progress phase with their names.
    /// 2. Resolves up to four sources concurrently; on failure, records the failing
    ///    service's phase (`building_git` or `resolving_image`) before returning.
    /// 3. Compiles the application with the combined resolution set.
    ///
    /// The whole sequence is bounded by `prepare_timeout`.
    async fn prepare(
        &self,
        environment: &EnvironmentId,
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
            let sources = stream::iter(
                pending
                    .into_iter()
                    .map(|(name, source)| self.prepare_source(environment, name, source)),
            )
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
            let resolved = compile_application(
                application,
                environment,
                self.instance_id.clone(),
                &resolutions,
            )
            .map_err(BoundaryError::Compilation)?;
            Ok(resolved)
        })
        .await
        .map_err(|source| {
            BoundaryError::Runtime(DockerError::unavailable("prepare application", source))
        })?
    }

    async fn local_images(&self) -> Result<piqueld_core::LocalImages, BoundaryError> {
        let images = DockerTimeout::Request
            .run("list images", self.docker.images())
            .await?;
        Ok(crate::docker::LocalImage::present(&images))
    }

    async fn check_available(&self) -> Result<(), BoundaryError> {
        DockerTimeout::Request
            .run("check Docker availability", self.docker.ensure_swarm(false))
            .await?;
        Ok(())
    }

    async fn observe(
        &self,
        application: &StoredEnvironment,
    ) -> Result<piqueld_core::ObservedApplication, BoundaryError> {
        DockerTimeout::Request
            .run("observe application", self.docker.observe(application.id()))
            .await
            .map_err(BoundaryError::from)
    }
}

impl<D: DockerApi> ApplicationRuntime<D> {
    /// Resolves one service source, journaling it as an action when progress is
    /// attached.
    ///
    /// Errors carry the service name and phase so `prepare` can report which service
    /// failed. Journal failures are reported the same way as resolution failures.
    async fn prepare_source(
        &self,
        environment: &EnvironmentId,
        name: piqueld_core::ServiceName,
        source: Source,
    ) -> Result<
        (piqueld_core::ServiceName, ResolvedSource),
        (piqueld_core::ServiceName, &'static str, BoundaryError),
    > {
        let phase = if matches!(source, Source::Git { .. }) {
            "building_git"
        } else {
            "resolving_image"
        };
        let journal = if let Some((store, id)) = &self.progress {
            Some(
                store
                    .begin_action(Some(id), phase, Some(name.as_str()))
                    .await
                    .map_err(|error| (name.clone(), phase, BoundaryError::Store(error)))?,
            )
        } else {
            None
        };
        let result = self
            .resolve_source(environment, name.as_str(), source)
            .await;
        if let (Some((store, _)), Some(journal)) = (&self.progress, &journal) {
            store
                .finish_action(
                    journal,
                    result.as_ref().err().map(BoundaryError::diagnostic),
                )
                .await
                .map_err(|error| (name.clone(), phase, BoundaryError::Store(error)))?;
        }
        result
            .map(|source| (name.clone(), source))
            .map_err(|error| (name, phase, error))
    }

    /// Pins an image reference to its registry digest, or checks out and builds a
    /// Git source into a local image.
    async fn resolve_source(
        &self,
        environment: &EnvironmentId,
        name: &str,
        source: Source,
    ) -> Result<ResolvedSource, BoundaryError> {
        match &source {
            Source::Image { image } => {
                let digest = DockerTimeout::ImageResolution
                    .run("resolve image", self.docker.resolve_image(image))
                    .await?;
                ResolvedSource::parse_image(image.clone(), digest).map_err(|source| {
                    BoundaryError::Runtime(DockerError::RequestSource {
                        operation: "validate resolved image",
                        source: Box::new(source),
                    })
                })
            }
            Source::Git { repository, build } => {
                // Deployments pin "self" before preparation; see `pin_manifest_sources`.
                let SourceRepository::Git(repository) = repository else {
                    return Err(BoundaryError::GitBuild(anyhow::anyhow!(
                        "the manifest repository source has no fetched manifest commit"
                    )));
                };
                let (commit, image_id) = self
                    .prepare_git(
                        environment,
                        name,
                        &source,
                        repository,
                        build,
                        self.docker.as_ref(),
                    )
                    .await
                    .map_err(BoundaryError::GitBuild)?;
                Ok(ResolvedSource::Git {
                    requested: source,
                    commit,
                    image_id,
                })
            }
        }
    }

    /// Checks out and builds a Git source, returning the commit and built image ID.
    ///
    /// During execution the build is recorded as a `BuildAttempt`: output is streamed
    /// into its log, a failure message is appended on error, and the attempt is
    /// finished as succeeded or failed. Without progress the build runs unrecorded.
    async fn prepare_git(
        &self,
        environment: &EnvironmentId,
        service: &str,
        source: &Source,
        repository: &piqueld_core::manifest::GitRepository,
        build: &piqueld_core::manifest::ValidatedBuild,
        docker: &D,
    ) -> anyhow::Result<(String, piqueld_core::resource::Sha256Digest)> {
        let Some((store, operation)) = &self.progress else {
            return crate::git::Checkout::prepare(
                repository,
                build,
                &self.instance_id,
                self.priority,
                docker,
            )
            .await;
        };
        let attempt = crate::build::BuildAttempt::start(
            Arc::clone(store),
            environment,
            operation,
            service,
            source,
            None,
        )
        .await?;
        let result = crate::git::Checkout::prepare_recorded(
            repository,
            build,
            &self.instance_id,
            self.priority,
            docker,
            Some(&attempt.log),
        )
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
                    .append(
                        format!("\nERROR Build failed: {error}\n").as_bytes(),
                        piqueld_core::api::LogStream::Stderr,
                    )
                    .await?;
                attempt
                    .finish(piqueld_core::api::BuildState::Failed, None)
                    .await?;
            }
        }
        result
    }
}
