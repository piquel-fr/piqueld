//! Previews: disposable deployments of a branch. Their status, logs,
//! deployments, and secrets use the environment methods with their ID.
pub use piqueld_core::api::{
    ApplicationPreviews, BranchState, CountedPreview, CreatePreviewRequest, CreatedPreview,
    DeletedPreview, LastDeployment, PreviewLimit, PreviewLimitReached, PreviewUsage, PreviewView,
    PrunePreviewsRequest,
};
pub use piqueld_core::manifest::PreviewLimits;
pub use piqueld_core::{EnvironmentKind, Preview, PreviewSlot, PreviewSlug};

use crate::{AcceptedOperation, Client, ClientError, client::generated_result};

impl Client {
    /// Lists an application's previews with their branch states.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn previews(&self, application: &str) -> Result<Vec<PreviewView>, ClientError> {
        generated_result(self.generated.list_previews(application).await)
            .await
            .map(|response| response.data)
    }

    /// Fetches one preview with its branch state.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn preview(&self, id: &str) -> Result<PreviewView, ClientError> {
        generated_result(self.generated.get_preview(id).await)
            .await
            .map(|response| response.data)
    }

    /// Creates and deploys a preview of a branch, or returns the existing
    /// preview of that branch and slot without redeploying it.
    ///
    /// # Errors
    /// Returns transport, API, decoding, branch, slot, or repository errors.
    pub async fn create_preview(
        &self,
        application: &str,
        request: &CreatePreviewRequest,
    ) -> Result<CreatedPreview, ClientError> {
        generated_result(
            self.generated
                .create_preview(application, None, request)
                .await,
        )
        .await
        .map(|response| response.data)
    }

    /// Deploys the head of a preview's branch.
    ///
    /// # Errors
    /// Returns transport, API, decoding, or repository errors.
    pub async fn deploy_preview(&self, id: &str) -> Result<AcceptedOperation, ClientError> {
        generated_result(self.generated.deploy_preview(id, None).await)
            .await
            .map(|response| response.data)
    }

    /// Deletes a preview with every volume it created.
    ///
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn delete_preview(&self, id: &str) -> Result<AcceptedOperation, ClientError> {
        generated_result(self.generated.delete_preview(id, None).await)
            .await
            .map(|response| response.data)
    }

    /// Deletes the listed previews whose branch the repository confirms is
    /// gone, returning those whose deletion was accepted.
    ///
    /// # Errors
    /// Returns transport, API, decoding, or `repository_unavailable` errors.
    pub async fn prune_previews(
        &self,
        application: &str,
        request: &PrunePreviewsRequest,
    ) -> Result<Vec<DeletedPreview>, ClientError> {
        generated_result(self.generated.prune_previews(application, request).await)
            .await
            .map(|response| response.data)
    }
}
