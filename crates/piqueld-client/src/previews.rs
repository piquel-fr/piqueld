//! Previews: disposable deployments of a branch. Their status, logs,
//! deployments, and secrets use the environment methods with their ID.
pub use piqueld_core::api::{
    ApplicationPreviews, BranchState, CountedPreview, CreatePreviewRequest, CreatedPreview,
    DeletedPreview, LastDeployment, LocalSnapshot, PreviewLimit, PreviewLimitReached, PreviewUsage,
    PreviewView, PrunePreviewsRequest,
};
pub use piqueld_core::manifest::PreviewLimits;
pub use piqueld_core::{EnvironmentKind, Preview, PreviewSlot, PreviewSlug};

use crate::{AcceptedOperation, Client, ClientError, client::EnvelopedCall};

impl Client {
    /// Lists an application's previews with their branch states.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn previews(&self, application: &str) -> Result<Vec<PreviewView>, ClientError> {
        self.generated.list_previews(application).data().await
    }

    /// Fetches one preview with its branch state.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn preview(&self, id: &str) -> Result<PreviewView, ClientError> {
        self.generated.get_preview(id).data().await
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
        self.generated
            .create_preview(application, None, request)
            .data()
            .await
    }

    /// Deploys the head of a preview's branch.
    ///
    /// # Errors
    /// Returns transport, API, decoding, or repository errors.
    pub async fn deploy_preview(&self, id: &str) -> Result<AcceptedOperation, ClientError> {
        self.generated.deploy_preview(id, None).data().await
    }

    /// Uploads `bundle`, a Git bundle of `length` bytes holding a snapshot
    /// of a local worktree of `branch`, and deploys it to the preview of
    /// `branch` and `slot`, creating that preview first if needed.
    ///
    /// The bundle is streamed with its `Content-Length`, so the daemon
    /// refuses one over its upload limit before reading it.
    ///
    /// # Errors
    /// Returns transport, API, or decoding errors, including
    /// `snapshot_too_large`, `snapshot_invalid`, and `snapshot_base_unavailable`.
    pub async fn deploy_local_preview(
        &self,
        application: &str,
        (branch, slot): (&str, Option<&str>),
        (bundle, length): (impl Into<reqwest::Body>, u64),
    ) -> Result<CreatedPreview, ClientError> {
        let mut query = vec![("branch", branch.to_owned())];
        query.extend(slot.map(|slot| ("slot", slot.to_owned())));
        self.send(
            &format!(
                "/api/v1/applications/{}/previews/local",
                progenitor_client::encode_path(application)
            ),
            &query,
            &[("Content-Length", length.to_string())],
            ("application/octet-stream", bundle.into()),
        )
        .await
    }

    /// Deletes a preview with every volume it created.
    ///
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn delete_preview(&self, id: &str) -> Result<AcceptedOperation, ClientError> {
        self.generated.delete_preview(id, None).data().await
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
        self.generated
            .prune_previews(application, request)
            .data()
            .await
    }
}
