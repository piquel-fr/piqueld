//! Application secret metadata and write-only secret values.
use crate::{Client, ClientError, SecretMetadata, client::generated_result};
impl Client {
    /// Recovers from a lost master key by discarding every application's stored values.
    /// # Errors
    /// Returns an API error if the current key still works, or transport errors.
    pub async fn recover_secret_key(&self) -> Result<crate::SecretKeyRecovery, ClientError> {
        generated_result(self.generated.recover_secret_key().await)
            .await
            .map(|response| response.data)
    }

    /// Lists metadata without retrieving secret values.
    /// # Errors
    /// Returns transport, decoding or API errors.
    pub async fn secrets(&self, application: &str) -> Result<Vec<SecretMetadata>, ClientError> {
        generated_result(self.generated.application_secrets(application).await)
            .await
            .map(|response| response.data)
    }
    /// Creates or rotates an application secret with optimistic version checking.
    /// # Errors
    /// Returns transport, decoding or API errors; values are never returned.
    pub async fn put_secret(
        &self,
        application: &str,
        name: &str,
        generation: i64,
        value: Vec<u8>,
    ) -> Result<SecretMetadata, ClientError> {
        generated_result(
            self.generated
                .put_application_secret(application, name, generation, value)
                .await,
        )
        .await
        .map(|response| response.data)
    }
    /// Deletes an unreferenced secret and its Docker versions.
    /// # Errors
    /// Returns version/reference conflicts or transport/storage errors.
    pub async fn delete_secret(
        &self,
        application: &str,
        name: &str,
        generation: i64,
    ) -> Result<(), ClientError> {
        generated_result(
            self.generated
                .delete_application_secret(application, name, generation)
                .await,
        )
        .await
        .map(|_| ())
    }
}
