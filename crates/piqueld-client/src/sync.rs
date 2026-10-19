//! Deploying on push: per-environment opt-out and GitHub webhooks. How an
//! application syncs is a repository setting, changed with
//! [`ApplicationEdit::RepositorySync`](crate::edit::ApplicationEdit::RepositorySync).
pub use piqueld_core::sync::{
    EnvironmentSyncRequest, RepositorySync, SyncCheck, SyncState, SyncedHead, SystemActor,
    WebhookSecret, WebhookView, short_commit,
};

use crate::{Client, ClientError, EnvironmentView, client::EnvelopedCall};

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
        self.generated
            .set_environment_sync(id, None, &request)
            .data()
            .await
    }

    /// Reads an application's webhook payload URL and secret state.
    ///
    /// # Errors
    /// Returns transport, API, or decoding errors.
    pub async fn webhook(&self, application: &str) -> Result<WebhookView, ClientError> {
        self.generated.get_webhook(application).data().await
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
        self.generated
            .generate_webhook_secret(application)
            .data()
            .await
    }
}
