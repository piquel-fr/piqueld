//! Environment secret metadata and write-only secret values.
use crate::{Client, ClientError, SecretMetadata, client::generated_result};
impl Client {
    /// Recovers from a lost master key by discarding every environment's stored values.
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
    pub async fn secrets(&self, environment: &str) -> Result<Vec<SecretMetadata>, ClientError> {
        generated_result(self.generated.environment_secrets(environment).await)
            .await
            .map(|response| response.data)
    }
    /// Creates or rotates an environment secret with optimistic version checking.
    /// # Errors
    /// Returns transport, decoding or API errors; values are never returned.
    pub async fn put_secret(
        &self,
        environment: &str,
        name: &str,
        generation: i64,
        value: Vec<u8>,
    ) -> Result<SecretMetadata, ClientError> {
        generated_result(
            self.generated
                .put_environment_secret(environment, name, generation, value)
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
        environment: &str,
        name: &str,
        generation: i64,
    ) -> Result<(), ClientError> {
        generated_result(
            self.generated
                .delete_environment_secret(environment, name, generation)
                .await,
        )
        .await
        .map(|_| ())
    }
}
