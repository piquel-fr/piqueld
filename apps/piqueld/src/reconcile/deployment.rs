//! Fetch a candidate manifest without changing accepted intent or runtime state.
use super::{Controller, DockerApi, Operation, OperationError, StoredEnvironment};
use crate::store::StoreError;
use piqueld_core::{
    NormalizedApplication,
    api::DiagnosticView,
    manifest::{ApplicationManifest, GitRevision, RenderContext, RepositoryManifest},
};

impl<D: DockerApi> Controller<D> {
    /// Resolves the rendered manifest an operation should deploy, with `"self"`
    /// sources pinned to the commit its manifest was fetched from.
    ///
    /// Deployments are rendered when captured, or once fetched when
    /// repository-backed, and retries reuse that rendering, so they never
    /// re-read variables or a moving branch. Otherwise the repository is cloned
    /// and the manifest file (at most 2 MiB; `.json` parsed as JSON, anything
    /// else as TOML) is validated, required to keep the application name,
    /// rendered for `environment` at the fetched commit, and persisted with it.
    /// Its own `spec.manifest` is ignored, even when absent or invalid: the
    /// repository it was fetched from replaces it before validation, with a
    /// `manifest_connection_ignored` warning when it names another repository
    /// URL or manifest path. Inputs without repository backing are marked
    /// fetched as-is.
    pub(super) async fn deployment_manifest(
        &self,
        operation: &Operation,
        environment: &StoredEnvironment,
    ) -> Result<NormalizedApplication, OperationError> {
        let snapshot = self.store.deployment_snapshot(&operation.id).await?;
        let Some(input) = self.store.deployment_input(operation).await? else {
            return Ok(snapshot.rendering.ok_or(StoreError::Corrupt)?.application);
        };
        if input.fetched {
            let application = snapshot.rendering.ok_or(StoreError::Corrupt)?.application;
            return Ok(match &input.commit {
                Some(commit) => application.pin_manifest_sources(commit),
                None => application,
            });
        }
        let Some(backing) = &input.template.spec().manifest else {
            let rendering = snapshot.rendering.ok_or(StoreError::Corrupt)?;
            self.store
                .save_deployment_input(operation, &input.template, &rendering, None, &[])
                .await?;
            return Ok(rendering.application);
        };
        self.store
            .progress(&operation.id, "fetching_manifest", None)
            .await?;
        let checkout = crate::git::Checkout::clone(&backing.repository)
            .await
            .map_err(|error| {
                tracing::error!(?error, "manifest repository fetch failed");
                OperationError::ManifestFetchFailed(error)
            })?;
        let mut manifest = Self::read_manifest(&checkout, &backing.path).await?;
        // Replaced before validation: `self` sources and `git.*` references
        // need a connection, and the file's own may be absent or invalid.
        let declared = manifest.spec.manifest.replace(backing.clone());
        let warnings = Self::ignored_connection(declared.as_ref(), backing)
            .into_iter()
            .collect::<Vec<_>>();
        let template = manifest
            .validate_template()
            .map_err(Self::invalid_manifest)?
            .normalize(input.template.id().clone());
        if template.metadata().name != input.template.metadata().name {
            return Err(OperationError::ManifestInvalid);
        }
        let rendering = template
            .render(&RenderContext {
                environment: environment.environment.name.clone(),
                git: Some(GitRevision {
                    branch: backing.repository.branch.clone(),
                    sha: checkout.commit.clone(),
                }),
                deployment: Some(operation.id.clone()),
            })
            .map_err(Self::invalid_manifest)?;
        self.check_current(operation).await?;
        self.store
            .save_deployment_input(
                operation,
                &template,
                &rendering,
                Some(&checkout.commit),
                &warnings,
            )
            .await?;
        Ok(rendering.application.pin_manifest_sources(&checkout.commit))
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
            let not_found = error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound);
            OperationError::ManifestInput {
                not_found,
                source: error,
            }
        })?;
        let metadata =
            tokio::fs::metadata(&path)
                .await
                .map_err(|error| OperationError::ManifestInput {
                    not_found: true,
                    source: error.into(),
                })?;
        if !metadata.is_file() {
            return Err(OperationError::ManifestNotFound);
        }
        if metadata.len() > 2 * 1024 * 1024 {
            return Err(OperationError::ManifestInvalid);
        }
        let contents = tokio::fs::read_to_string(&path).await.map_err(|error| {
            tracing::error!(?error, "manifest read failed");
            OperationError::ManifestInput {
                not_found: false,
                source: error.into(),
            }
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
