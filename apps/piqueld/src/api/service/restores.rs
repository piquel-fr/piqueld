//! Restoring an earlier deployment of an environment or preview exactly as it
//! ran. Nothing is rendered, fetched or built: its saved target is deployed
//! again with the secret versions it pinned, and its jobs never run again.
use super::{ApplicationError, ApplicationService, Mutation, MutationResponse, Restore};
use crate::store::StoreError;
use piqueld_core::api::AcceptedOperation;

impl ApplicationService {
    /// Restores a deployment of an environment or preview, after the
    /// caller's idempotency key and permission: it must have succeeded
    /// (`deployment_not_restorable`), its images must be present, pulled again
    /// by digest when they can be (`image_unavailable`, naming the service and
    /// image), and the secret versions it pinned must still exist
    /// (`restore_secrets_unavailable`, naming them). Cleanup is held off from
    /// the image check until the restore is saved.
    ///
    /// # Errors
    /// Returns those failures, conflict, runtime, or storage errors.
    pub async fn restore(
        &self,
        actor: super::Actor<'_>,
        restore: Restore,
        request_id: Option<&str>,
    ) -> Result<AcceptedOperation, ApplicationError> {
        let (id, deployment) = restore.parts();
        let (id, deployment) = (id.clone(), deployment.to_owned());
        let request = Mutation::Restore(restore);
        request.check_submission(None, false, request_id)?;
        // A repeated request returns what it got, whatever changed since.
        if let Some(response) = self
            .store
            .replay(actor, &request, None, false, request_id)
            .await?
        {
            return Self::restore_response(response);
        }
        let target = self.store.restorable_target(&id, &deployment).await?;
        let sources = target
            .sources()
            .map(|(service, source)| (service.clone(), source.clone()))
            .collect();
        let _in_use = self.runtime.reuse_images(&sources).await?;
        // Boxed: acceptance would otherwise make every restore future large.
        let response = Box::pin(self.accept(actor, request, None, false, request_id)).await?;
        Self::restore_response(response)
    }

    /// The accepted operation a restore request responded with.
    fn restore_response(response: MutationResponse) -> Result<AcceptedOperation, ApplicationError> {
        match response {
            MutationResponse::Operation(accepted) => Ok(accepted),
            _ => Err(StoreError::Corrupt.into()),
        }
    }
}
