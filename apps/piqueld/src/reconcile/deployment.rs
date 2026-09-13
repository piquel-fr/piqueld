//! Fetch a candidate manifest without changing accepted intent or runtime state.
use super::{Controller, DockerApi, Operation, OperationError};
use piqueld_core::NormalizedApplication;

impl<D: DockerApi> Controller<D> {
    pub(super) async fn deployment_manifest(
        &self,
        operation: &Operation,
        current: &NormalizedApplication,
    ) -> Result<NormalizedApplication, OperationError> {
        let Some(input) = self.store.deployment_input(operation).await? else {
            return Ok(current.clone());
        };
        if input.fetched {
            return Ok(input.application);
        }
        let Some(backing) = &input.application.spec.manifest else {
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
                OperationError::ManifestFetchFailed
            })?;
        let path = checkout.path(&backing.path).await.map_err(|error| {
            tracing::error!(?error, "manifest file lookup failed");
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            {
                OperationError::ManifestNotFound
            } else {
                OperationError::ManifestInvalid
            }
        })?;
        let metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|_| OperationError::ManifestNotFound)?;
        if !metadata.is_file() {
            return Err(OperationError::ManifestNotFound);
        }
        if metadata.len() > 2 * 1024 * 1024 {
            return Err(OperationError::ManifestInvalid);
        }
        let contents = tokio::fs::read_to_string(&path).await.map_err(|error| {
            tracing::error!(?error, "manifest read failed");
            OperationError::ManifestInvalid
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
            OperationError::ManifestInvalid
        })?;
        let application = parsed.normalize(operation.application_id.clone());
        if application.metadata.name != input.application.metadata.name {
            return Err(OperationError::ManifestInvalid);
        }
        self.check_current(operation).await?;
        self.store
            .save_deployment_input(operation, &application, Some(&checkout.commit))
            .await?;
        Ok(application)
    }
}
