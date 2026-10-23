//! Fetch a candidate manifest without changing accepted intent or runtime state.
use super::{Controller, DockerApi, Operation, OperationError};
use crate::git::snapshot::SnapshotSource;
use crate::store::StoreError;
use crate::store::{DeploymentInput, Tracking};
use piqueld_core::{
    NormalizedApplication,
    api::{DeploymentOrigin, DiagnosticView},
    manifest::{
        ApplicationManifest, ApplicationTemplate, FileCaptureError, GitRevision, RenderContext,
        RepositoryManifest, ServiceFile,
    },
};

impl<D: DockerApi> Controller<D> {
    /// Resolves the rendered manifest an operation should deploy, with `"self"`
    /// sources pinned to the commit its manifest was fetched from, and, for a
    /// local snapshot, the snapshot they are checked out from.
    ///
    /// Deployments are rendered when captured, or once fetched when
    /// repository-backed, and retries reuse that rendering, so they never
    /// re-read variables, a moving branch, or the files services mount.
    /// Otherwise the repository is cloned and the manifest file (at most
    /// 2 MiB; `.json` parsed as JSON, anything else as TOML) is validated,
    /// required to keep the application name, and replaced by the captured
    /// saved manifest when unchanged (see [`Self::fetched_template`]), then
    /// captured with the repository files its services mount, rendered for
    /// `environment` at the fetched commit, bounded by `[previews]` when it
    /// is a preview, and persisted with it.
    /// Its own `spec.manifest` must decode but is ignored, even when absent or
    /// invalid: the repository it was fetched from replaces it before
    /// validation, with a `manifest_connection_ignored` warning when it names
    /// another repository URL or manifest path. Each `[previews]` bound is a
    /// warning too. It records the head of the branch the environment
    /// tracked (see [`Self::branch_head`]). Inputs without repository backing
    /// are marked fetched as-is. A local snapshot (`preview deploy --local`)
    /// is fetched the same way, from its own repository instead of the
    /// branch, which must still be the application's.
    pub(super) async fn deployment_manifest(
        &self,
        operation: &Operation,
        environment: Tracking<'_>,
    ) -> Result<(NormalizedApplication, Option<SnapshotSource>), OperationError> {
        let snapshot = self.store.deployment_snapshot(&operation.id).await?;
        let Some(input) = self.store.deployment_input(operation).await? else {
            return Ok((
                snapshot.rendering.ok_or(StoreError::Corrupt)?.application,
                None,
            ));
        };
        let backing = input.template.spec().manifest.as_ref();
        let source = self.snapshot_source(&input.origin, backing)?;
        if input.fetched {
            let application = snapshot.rendering.ok_or(StoreError::Corrupt)?.application;
            return Ok((
                match &input.commit {
                    Some(commit) => application.pin_manifest_sources(commit),
                    None => application,
                },
                source,
            ));
        }
        let Some(backing) = backing else {
            let rendering = snapshot.rendering.ok_or(StoreError::Corrupt)?;
            self.store
                .save_deployment_input(
                    operation,
                    (&input.template, None),
                    &rendering,
                    (None, None),
                    &[],
                )
                .await?;
            return Ok((rendering.application, None));
        };
        self.store
            .progress(&operation.id, "fetching_manifest", None)
            .await?;
        let checkout = self
            .store
            .checkouts()
            .checkout(
                source
                    .as_ref()
                    .map_or(&backing.repository, |source| &source.repository),
                None,
            )
            .await
            .map_err(|error| {
                tracing::error!(?error, "manifest repository fetch failed");
                OperationError::ManifestFetchFailed(error)
            })?;
        let (template, fetched, mut warnings) = Box::pin(self.fetched_template(
            operation,
            &environment.environment().kind,
            (&checkout, backing),
            &input,
        ))
        .await?;
        let mut rendering = template
            .render(&RenderContext {
                target: environment.environment().target(),
                git: Some(GitRevision {
                    branch: backing.repository.branch.clone(),
                    sha: checkout.commit.clone(),
                }),
                deployment: Some(operation.id.clone()),
            })
            .map_err(Self::invalid_manifest)?;
        warnings.extend(
            environment
                .environment()
                .kind
                .bound(&mut rendering.application, self.store.preview_limits())
                .into_iter()
                .map(DiagnosticView::from),
        );
        self.check_current(operation).await?;
        let branch_head = self
            .branch_head(
                operation,
                input.tracked_branch.as_deref(),
                backing,
                (&checkout, source.is_some()),
            )
            .await?;
        self.store
            .save_deployment_input(
                operation,
                (&template, Some(&fetched)),
                &rendering,
                (Some(&checkout.commit), branch_head.as_ref()),
                &warnings,
            )
            .await?;
        Ok((
            rendering.application.pin_manifest_sources(&checkout.commit),
            source,
        ))
    }

    /// Reads the manifest at `backing`'s path in `checkout` and validates
    /// it as `input`'s application, with its connection and name. A
    /// deployment of `kind` reading the application's connection deploys
    /// the saved manifest it captured instead while the one read is the one
    /// the application last read: only a push changing the manifest replaces
    /// edits made since. Returns the manifest to deploy, with the repository
    /// files it mounts, the one read, and a
    /// `manifest_connection_ignored` warning when the file names another
    /// connection.
    async fn fetched_template(
        &self,
        operation: &Operation,
        kind: &piqueld_core::EnvironmentKind,
        (checkout, backing): (&crate::git::Checkout, &RepositoryManifest),
        input: &DeploymentInput,
    ) -> Result<
        (
            ApplicationTemplate,
            ApplicationTemplate,
            Vec<DiagnosticView>,
        ),
        OperationError,
    > {
        let mut manifest = Self::read_manifest(checkout, &backing.path).await?;
        // Replaced before validation: `self` sources and `git.*` references
        // need a connection, and the file's own may be absent or invalid.
        let declared = manifest.spec.manifest.replace(backing.clone());
        let warnings = Self::ignored_connection(declared.as_ref(), backing)
            .into_iter()
            .collect();
        let template = manifest
            .validate_template()
            .map_err(Self::invalid_manifest)?
            .normalize(input.template.id().clone());
        if template.metadata().name != input.template.metadata().name {
            return Err(OperationError::ManifestInvalid);
        }
        let deployed = if input.unchanged(&template)
            && self
                .store
                .reads_connection(operation, kind, &template)
                .await?
        {
            input.template.clone()
        } else {
            template.clone()
        };
        let deployed = Self::capture_files(checkout, deployed).await?;
        Ok((deployed, template, warnings))
    }

    /// The kept snapshot a deployment of `origin` from the repository of
    /// `backing` checks out, if it is a local snapshot.
    fn snapshot_source(
        &self,
        origin: &DeploymentOrigin,
        backing: Option<&RepositoryManifest>,
    ) -> Result<Option<SnapshotSource>, OperationError> {
        match (origin, backing) {
            (DeploymentOrigin::Local(local), Some(backing)) => Ok(Some(
                self.store
                    .snapshots()
                    .source(&backing.repository.url, &local.commit),
            )),
            (DeploymentOrigin::Local(_), None) => Err(StoreError::Corrupt.into()),
            _ => Ok(None),
        }
    }

    /// The head of `tracked`, the branch the environment tracked unpinned
    /// when this deployment was requested, as this deployment finds it: what sync follows from, so pushes deploy once the
    /// branch moves past it. A deployment of that head fetched it. A one-off
    /// deployment of another revision, or of a `local` snapshot, lists the
    /// branch, so it stays until the branch moves; when the branch cannot be
    /// listed, the fetch fails, as a failed clone does. When it is gone, a
    /// one-off records nothing, while a snapshot records its absence, so
    /// pushing the branch is a move. Sync's own deployments recorded their
    /// head when requested.
    async fn branch_head(
        &self,
        operation: &Operation,
        tracked: Option<&str>,
        backing: &RepositoryManifest,
        (checkout, local): (&crate::git::Checkout, bool),
    ) -> Result<Option<crate::store::BranchHead>, OperationError> {
        let Some(tracked) = tracked else {
            return Ok(None);
        };
        if self.store.requested_by_sync(operation).await? {
            return Ok(None);
        }
        let url = backing.repository.url.clone();
        let commit = if !local
            && backing.repository.commit.is_none()
            && backing.repository.branch == tracked
        {
            Some(checkout.commit.clone())
        } else {
            // Without its head, the previous one would replace this deployment
            // at the next sync, so the fetch fails, as a failed clone does.
            crate::git::Heads::list(&url)
                .await
                .map_err(|error| {
                    tracing::error!(?error, "listing the tracked branch failed");
                    OperationError::ManifestFetchFailed(error)
                })?
                .head(tracked)
                .map(str::to_owned)
        };
        Ok(
            (commit.is_some() || local).then(|| crate::store::BranchHead {
                url,
                branch: tracked.to_owned(),
                commit,
            }),
        )
    }

    /// Captures every repository file `template`'s services mount, as
    /// `checkout` holds it, with the template.
    async fn capture_files(
        checkout: &crate::git::Checkout,
        template: ApplicationTemplate,
    ) -> Result<ApplicationTemplate, OperationError> {
        let mut files = Vec::new();
        // Stop at the budget, rather than read every file first.
        let mut total = template.spec().inline_file_bytes();
        for relative in template.spec().repository_file_paths() {
            let content = Self::read_file(checkout, relative).await?;
            total += content.len();
            if total > ServiceFile::MAX_TOTAL_BYTES {
                return Err(FileCaptureError::TotalTooLarge.into());
            }
            files.push((relative.to_owned(), content));
        }
        Ok(template.with_files(files.into_iter().collect())?)
    }

    /// Reads the regular file at `relative` in `checkout` as UTF-8 text of at
    /// most [`ServiceFile::MAX_BYTES`].
    async fn read_file(
        checkout: &crate::git::Checkout,
        relative: &str,
    ) -> Result<String, OperationError> {
        let path = checkout.path(relative).await.map_err(|error| {
            let missing = error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound);
            if missing {
                FileCaptureError::Missing(relative.into())
            } else {
                FileCaptureError::NotAFile(relative.into())
            }
        })?;
        let unreadable = |error: std::io::Error| {
            tracing::error!(?error, "repository file read failed");
            OperationError::ManifestInput {
                not_found: false,
                source: error.into(),
            }
        };
        let metadata = tokio::fs::metadata(&path).await.map_err(unreadable)?;
        if !metadata.is_file() {
            return Err(FileCaptureError::NotAFile(relative.into()).into());
        }
        if metadata.len() > ServiceFile::MAX_BYTES as u64 {
            return Err(FileCaptureError::TooLarge(relative.into()).into());
        }
        let bytes = tokio::fs::read(&path).await.map_err(unreadable)?;
        String::from_utf8(bytes).map_err(|_| FileCaptureError::NotText(relative.into()).into())
    }

    /// Reads and decodes, without validating, the manifest file at `relative`
    /// in `checkout`: at most 2 MiB, `.json` parsed as JSON and anything else
    /// as TOML.
    async fn read_manifest(
        checkout: &crate::git::Checkout,
        relative: &str,
    ) -> Result<ApplicationManifest, OperationError> {
        let path = checkout.path(relative).await.map_err(|error| {
            tracing::error!(?error, "manifest file lookup failed");
            OperationError::manifest_input(error)
        })?;
        let metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|error| OperationError::manifest_input(error.into()))?;
        if !metadata.is_file() {
            return Err(OperationError::ManifestNotFound);
        }
        if metadata.len() > piqueld_core::api::MAX_REQUEST_BODY_BYTES as u64 {
            return Err(OperationError::ManifestInvalid);
        }
        let contents = tokio::fs::read_to_string(&path).await.map_err(|error| {
            tracing::error!(?error, "manifest read failed");
            OperationError::manifest_input(error.into())
        })?;
        if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        {
            ApplicationManifest::decode_json(&contents)
        } else {
            ApplicationManifest::decode_toml(&contents)
        }
        .map_err(Self::invalid_manifest)
    }

    /// Warns that a fetched file's `spec.manifest` is ignored when it names
    /// another repository URL or manifest path than `fetched` from. Branches
    /// and commits differ between environments, so they are not compared.
    fn ignored_connection(
        declared: Option<&RepositoryManifest>,
        fetched: &RepositoryManifest,
    ) -> Option<DiagnosticView> {
        let declared = declared?;
        if declared.repository.url == fetched.repository.url && declared.path == fetched.path {
            return None;
        }
        tracing::warn!("fetched manifest names another repository connection");
        Some(DiagnosticView {
            code: "manifest_connection_ignored".into(),
            message: format!(
                "The fetched manifest's spec.manifest names {} in {}, which is ignored: this application reads {} from {}. Change the connection with `piquelctl app repository`.",
                declared.path, declared.repository.url, fetched.path, fetched.repository.url
            ),
        })
    }

    /// Reports an invalid repository manifest.
    fn invalid_manifest(error: piqueld_core::ValidationErrors) -> OperationError {
        tracing::error!(?error, "repository manifest validation failed");
        OperationError::ManifestInput {
            not_found: false,
            source: error.into(),
        }
    }
}
