//! Fetch a candidate manifest without changing accepted intent or runtime state.
use super::{Controller, DockerApi, Operation, OperationError, StoredEnvironment};
use crate::store::StoreError;
use piqueld_core::{
    NormalizedApplication,
    manifest::{GitRevision, RenderContext},
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
    /// Inputs without repository backing are marked fetched as-is.
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
                .save_deployment_input(operation, &input.template, &rendering, None)
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
        let parsed = Self::read_manifest(&checkout, &backing.path).await?;
        let template = parsed.normalize(input.template.id().clone());
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
        let application = &rendering.application;
        // Retries pin "self" from the stored manifest and commit alone, so the
        // manifest must name the repository that commit was fetched from.
        let declared = application.spec().manifest.as_ref();
        if application.spec().builds_from_manifest()
            && declared.map(|manifest| &manifest.repository.url) != Some(&backing.repository.url)
        {
            tracing::error!("manifest with \"self\" sources names another repository");
            return Err(OperationError::ManifestInput {
                not_found: false,
                source: anyhow::anyhow!(
                    "\"self\" sources require spec.manifest.repository.url to be {}",
                    backing.repository.url
                ),
            });
        }
        self.check_current(operation).await?;
        self.store
            .save_deployment_input(operation, &template, &rendering, Some(&checkout.commit))
            .await?;
        Ok(rendering.application.pin_manifest_sources(&checkout.commit))
    }

    /// Reads and validates the manifest file at `relative` in `checkout`: at
    /// most 2 MiB, `.json` parsed as JSON and anything else as TOML.
    async fn read_manifest(
        checkout: &crate::git::Checkout,
        relative: &str,
    ) -> Result<piqueld_core::manifest::ValidatedTemplate, OperationError> {
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
            piqueld_core::manifest::parse_template_json(&contents)
        } else {
            piqueld_core::manifest::parse_template_toml(&contents)
        }
        .map_err(Self::invalid_manifest)
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
