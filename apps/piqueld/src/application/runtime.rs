//! Resolves accepted manifest inputs during operation execution.

use super::{BoundaryError, RuntimeBoundary};
use crate::reconcile::ImagesInUse;
use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt, stream};
use piqueld_core::{
    EnvironmentId, EnvironmentKind, InstanceId, NormalizedApplication, ResolutionSet,
    compile_application,
    manifest::{SourceRepository, ValidatedSource as Source},
    resource::ResolvedSource,
};
use piqueld_docker::{BuildPriority, DockerApi, DockerError, DockerTimeout};
use piqueld_store::StoredEnvironment;
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
    /// Shared with the reconciler loop so mutations can request a full scan.
    rescan: Arc<Notify>,
    /// Upper bound for resolving every source and compiling one application.
    prepare_timeout: Duration,
    // Set only for execution, never API previews. Records the source-preparation
    // phase and service names, and journals each source-preparation action.
    progress: Option<(Arc<piqueld_store::Store>, String)>,
    /// The build queue its Git sources wait in.
    priority: BuildPriority,
    /// Whether Git sources build even when an earlier build's image could
    /// be reused.
    rebuild: bool,
    /// The local snapshot `"self"` sources are checked out from, if any.
    snapshot: Option<crate::git::snapshot::SnapshotSource>,
    /// The lock image cleanup takes exclusively; see `ImagesInUse`.
    images: Arc<tokio::sync::RwLock<()>>,
    /// The controller's lock around Docker mutations; see `lock_mutations`.
    mutations: Arc<tokio::sync::Mutex<()>>,
    /// Admission for the checkouts Git sources build in.
    checkouts: Arc<crate::git::Checkouts>,
}

impl<D> ApplicationRuntime<D> {
    /// Creates application runtime orchestration for `store`'s instance with
    /// the supplied preparation budget, sharing `images`, the lock image cleanup takes,
    /// `mutations`, the controller's lock around Docker mutations, and
    /// the checkout admission of `sources`.
    #[must_use]
    pub fn new(
        docker: Arc<D>,
        store: &piqueld_store::Store,
        sources: &crate::git::Sources,
        wake: Arc<Notify>,
        prepare_timeout: Duration,
        images: Arc<tokio::sync::RwLock<()>>,
        mutations: Arc<tokio::sync::Mutex<()>>,
    ) -> Self {
        Self {
            docker,
            instance_id: store.instance_id().clone(),
            wake,
            rescan: Arc::new(Notify::new()),
            prepare_timeout,
            progress: None,
            priority: BuildPriority::Environment,
            rebuild: false,
            snapshot: None,
            images,
            mutations,
            checkouts: Arc::clone(sources.checkouts()),
        }
    }
    /// Requests full scans through `rescan`, shared with the reconciler.
    #[must_use]
    pub(crate) fn with_rescan(mut self, rescan: Arc<Notify>) -> Self {
        self.rescan = rescan;
        self
    }

    /// Associates source preparation with the operation whose status is reported.
    pub(crate) fn with_progress(
        mut self,
        store: Arc<piqueld_store::Store>,
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

    /// Builds every Git source when `rebuild` is set, instead of reusing the
    /// image of an earlier build with the same commit and build settings.
    #[must_use]
    pub(crate) const fn with_rebuild(mut self, rebuild: bool) -> Self {
        self.rebuild = rebuild;
        self
    }

    /// Checks `"self"` sources out from a local snapshot, if any.
    #[must_use]
    pub(crate) fn with_snapshot(
        mut self,
        snapshot: Option<crate::git::snapshot::SnapshotSource>,
    ) -> Self {
        self.snapshot = snapshot;
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

    async fn lock_mutations(&self) -> tokio::sync::OwnedMutexGuard<()> {
        Arc::clone(&self.mutations).lock_owned().await
    }

    /// Removes secrets only when they carry this instance's and application's
    /// ownership labels, so foreign Docker secrets are never deleted.
    async fn remove_secrets(
        &self,
        application: &piqueld_core::EnvironmentId,
        names: &[String],
    ) -> Result<(), BoundaryError> {
        let ownership = piqueld_core::resource::Ownership::scope(&self.instance_id, application);
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
    ) -> Result<Option<piqueld_docker::Exec>, BoundaryError> {
        Ok(self
            .docker
            .create_exec(&self.instance_id, environment, request)
            .await?)
    }
    async fn run_exec(
        &self,
        exec: &piqueld_docker::Exec,
        io: piqueld_docker::ExecIo,
    ) -> Result<i64, BoundaryError> {
        Ok(self.docker.run_exec(exec, io).await?)
    }
    fn trigger_reconciliation(&self) {
        self.wake.notify_one();
    }
    fn trigger_full_scan(&self) {
        self.rescan.notify_one();
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
        Ok(piqueld_docker::LocalImage::present(&images))
    }

    async fn reuse_images(
        &self,
        sources: &std::collections::BTreeMap<piqueld_core::ServiceName, ResolvedSource>,
    ) -> Result<ImagesInUse, BoundaryError> {
        let in_use = ImagesInUse::hold(Arc::clone(&self.images)).await;
        // Bounded like preparation: a hung pull would hold cleanup off.
        tokio::time::timeout(self.prepare_timeout, in_use.ensure(&*self.docker, sources))
            .await
            .map_err(|source| {
                BoundaryError::Runtime(DockerError::unavailable("reuse release images", source))
            })?
            .map_err(|error| match error {
                crate::operations::OperationError::Docker(error) => BoundaryError::Runtime(error),
                error => BoundaryError::ImageUnavailable(Box::new(error)),
            })?;
        Ok(in_use)
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

    async fn containers(&self) -> Result<Vec<piqueld_docker::ManagedContainer>, BoundaryError> {
        Ok(self.docker.managed_containers(&self.instance_id).await?)
    }

    async fn container_usage(
        &self,
        id: &str,
    ) -> Result<Option<piqueld_docker::ContainerUsage>, BoundaryError> {
        Ok(self.docker.container_usage(id).await?)
    }

    fn container_logs(&self, id: &str, since: i32) -> piqueld_docker::ContainerLogs {
        self.docker.container_logs(id, since)
    }

    fn container_starts(&self, since: i64) -> piqueld_docker::ContainerStarts {
        self.docker.container_starts(since)
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
                    .prepare_git(environment, name, &source, repository, build)
                    .await
                    // An unreachable build machine is not a failed build.
                    .map_err(|error| {
                        if matches!(
                            error.downcast_ref(),
                            Some(DockerError::BuilderUnavailable(_))
                        ) {
                            error
                                .downcast()
                                .map_or_else(BoundaryError::GitBuild, BoundaryError::Runtime)
                        } else {
                            BoundaryError::GitBuild(error)
                        }
                    })?;
                Ok(ResolvedSource::Git {
                    requested: source,
                    commit,
                    image_id,
                })
            }
        }
    }

    /// Checks out a Git source and builds it, or reuses the image an earlier
    /// build produced from the same commit and build settings, returning the
    /// commit and image ID.
    ///
    /// During execution the build is recorded as a `BuildAttempt`: output is streamed
    /// into its log, a failure message is appended on error, and the attempt is
    /// finished as succeeded, naming the build it reused, or failed. Without
    /// progress the build runs unrecorded, and reuses nothing.
    async fn prepare_git(
        &self,
        environment: &EnvironmentId,
        service: &str,
        source: &Source,
        repository: &piqueld_core::manifest::GitRepository,
        build: &piqueld_core::manifest::ValidatedBuild,
    ) -> anyhow::Result<(String, piqueld_core::BuiltImage)> {
        let docker = self.docker.as_ref();
        let repository =
            crate::git::snapshot::SnapshotSource::checkout(self.snapshot.as_ref(), repository);
        let owner = (&self.instance_id, self.priority);
        let Some((store, operation)) = &self.progress else {
            let checkout = self.checkouts.checkout(repository, None).await?;
            let image = checkout.build(build, owner, docker, None).await?;
            return Ok((checkout.commit, image));
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
        let log = &attempt.log;
        let result = async {
            let checkout = self.checkouts.checkout(repository, Some(log)).await?;
            log.commit(&checkout.commit).await?;
            let image = match self
                .reusable_build(store, environment, source, &checkout.commit)
                .await?
            {
                Some(earlier) => GitImage::Reused(earlier),
                None => GitImage::Built(checkout.build(build, owner, docker, Some(log)).await?),
            };
            anyhow::Ok((checkout.commit, image))
        }
        .await;
        match result {
            Ok((commit, GitImage::Built(image))) => {
                attempt
                    .finish(
                        piqueld_core::api::BuildState::Succeeded,
                        Some(image.as_str()),
                    )
                    .await?;
                Ok((commit, image))
            }
            Ok((commit, GitImage::Reused(earlier))) => {
                attempt.reuse(&earlier).await?;
                Ok((commit, earlier.image))
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
                Err(error)
            }
        }
    }

    /// The newest earlier build whose image a build of `source` at `commit`
    /// can use instead of building: one of the same application with the
    /// same commit and build settings, whose image the engine still has.
    /// None when the deployment asked for a rebuild.
    async fn reusable_build(
        &self,
        store: &piqueld_store::Store,
        environment: &EnvironmentId,
        source: &Source,
        commit: &str,
    ) -> anyhow::Result<Option<piqueld_store::ReusableBuild>> {
        if self.rebuild {
            return Ok(None);
        }
        let earlier = store.reusable_builds(environment, source, commit).await?;
        if earlier.is_empty() {
            return Ok(None);
        }
        let images = DockerTimeout::Request
            .run("list images", self.docker.images())
            .await?;
        let present = piqueld_docker::LocalImage::present(&images);
        Ok(earlier
            .into_iter()
            .find(|build| present.contains(&build.image.clone().into())))
    }
}

/// Where the image of a Git source came from.
enum GitImage {
    /// Built from the checkout.
    Built(piqueld_core::BuiltImage),
    /// Reused from an earlier build of the same commit and build settings.
    Reused(piqueld_store::ReusableBuild),
}
