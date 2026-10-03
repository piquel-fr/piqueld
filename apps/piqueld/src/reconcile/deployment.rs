//! Fetch a candidate manifest without changing accepted intent or runtime state.
use super::{Controller, DockerApi, Operation, OperationError};
use piqueld_core::NormalizedApplication;

impl<D: DockerApi> Controller<D> {
    /// Resolves the manifest an operation should deploy, with `"self"` sources
    /// pinned to the commit its manifest was fetched from.
    ///
    /// Operations without a captured deployment input use `current`. A fetched
    /// input is reused so retries never re-read a moving branch. Otherwise, when
    /// the application is repository-backed, the repository is cloned and the
    /// manifest file (at most 2 MiB; `.json` parsed as JSON, anything else as
    /// TOML) is validated, required to keep the application name, and persisted
    /// with its commit. Inputs without repository backing are marked fetched as-is.
    pub(super) async fn deployment_manifest(
        &self,
        operation: &Operation,
        current: &NormalizedApplication,
    ) -> Result<NormalizedApplication, OperationError> {
        let Some(input) = self.store.deployment_input(operation).await? else {
            return Ok(current.clone());
        };
        if input.fetched {
            return Ok(match &input.commit {
                Some(commit) => input.application.pin_manifest_sources(commit),
                None => input.application,
            });
        }
        let Some(backing) = &input.application.spec().manifest else {
            self.store
                .save_deployment_input(operation, &input.application, None)
                .await?;
            return Ok(input.application);
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
        let path = checkout.path(&backing.path).await.map_err(|error| {
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
        let parsed = if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        {
            piqueld_core::parse_json(&contents)
        } else {
            piqueld_core::parse_toml(&contents)
        }
        .map_err(|error| {
            tracing::error!(?error, "repository manifest validation failed");
            OperationError::ManifestInput {
                not_found: false,
                source: error.into(),
            }
        })?;
        let application = parsed.normalize(operation.application_id.clone());
        if application.metadata().name != input.application.metadata().name {
            return Err(OperationError::ManifestInvalid);
        }
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
            .save_deployment_input(operation, &application, Some(&checkout.commit))
            .await?;
        Ok(application.pin_manifest_sources(&checkout.commit))
    }
}
