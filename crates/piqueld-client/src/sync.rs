//! Deploying on push: per-environment opt-out and GitHub webhooks. How an
//! application syncs is a repository setting, changed with
//! [`ApplicationEdit::RepositorySync`](crate::ApplicationEdit).
pub use piqueld_core::sync::{
    EnvironmentSyncRequest, RepositorySync, SyncCheck, SyncState, SyncedHead, SystemActor,
    WebhookSecret, WebhookView,
};

use crate::{Client, ClientError, EnvironmentView, client::generated_result};

impl Client {
    /// Opts an environment in or out of its application's sync.
    ///
    /// # Errors
    /// Returns transport, API, or decoding errors; previews are not found.
    pub async fn set_environment_sync(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<EnvironmentView, ClientError> {
        let request = EnvironmentSyncRequest { enabled };
        generated_result(
            self.generated
                .set_environment_sync(id, None, &request)
                .await,
        )
        .await
        .map(|response| response.data)
    }

    /// Reads an application's webhook payload URL and secret state.
    ///
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn webhook(&self, application: &str) -> Result<WebhookView, ClientError> {
        generated_result(self.generated.get_webhook(application).await)
            .await
            .map(|response| response.data)
    }

    /// Generates a new webhook secret for an application, replacing the
    /// previous one. The response is the only time it is shown.
    ///
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn generate_webhook_secret(
        &self,
        application: &str,
    ) -> Result<WebhookSecret, ClientError> {
        generated_result(self.generated.generate_webhook_secret(application).await)
            .await
            .map(|response| response.data)
    }
}
