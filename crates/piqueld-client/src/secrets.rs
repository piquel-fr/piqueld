use crate::{Client, ClientError, SecretMetadata, client::generated_result};
impl Client {
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
