//! Secret metadata and write-only secret values: generated secrets per
//! environment, and each application's store of manually set secrets.
use crate::{
    Client, ClientError, SecretAccess, SecretMetadata, StoredSecret, client::generated_result,
};
impl Client {
    /// Recovers from a lost master key by discarding every stored and generated value.
    /// # Errors
    /// Returns an API error if the current key still works, or transport errors.
    pub async fn recover_secret_key(&self) -> Result<crate::SecretKeyRecovery, ClientError> {
        generated_result(self.generated.recover_secret_key().await)
            .await
            .map(|response| response.data)
    }

    /// Lists an environment's generated secrets without retrieving values.
    /// # Errors
    /// Returns transport, decoding or API errors.
    pub async fn secrets(&self, environment: &str) -> Result<Vec<SecretMetadata>, ClientError> {
        generated_result(self.generated.environment_secrets(environment).await)
            .await
            .map(|response| response.data)
    }

    /// Lists an application's stored secrets and their access, never values.
    /// # Errors
    /// Returns transport, decoding or API errors.
    pub async fn stored_secrets(
        &self,
        application: &str,
    ) -> Result<Vec<StoredSecret>, ClientError> {
        generated_result(self.generated.application_secrets(application).await)
            .await
            .map(|response| response.data)
    }

    /// Creates or rotates a stored secret with optimistic version checking.
    /// `access` replaces its access list; a new secret otherwise allows every
    /// environment and no previews.
    /// # Errors
    /// Returns transport, decoding or API errors; values are never returned.
    pub async fn put_stored_secret(
        &self,
        application: &str,
        name: &str,
        generation: i64,
        value: Vec<u8>,
        access: Option<&SecretAccess>,
    ) -> Result<StoredSecret, ClientError> {
        let environments = access.map(|access| access.environments.to_query());
        generated_result(
            self.generated
                .put_application_secret(
                    application,
                    name,
                    environments.as_deref(),
                    access.map(|access| access.previews),
                    generation,
                    value,
                )
                .await,
        )
        .await
        .map(|response| response.data)
    }

    /// Replaces which environments, and whether previews, may mount a stored secret.
    /// # Errors
    /// Returns transport, decoding or API errors.
    pub async fn set_secret_access(
        &self,
        application: &str,
        name: &str,
        access: &SecretAccess,
    ) -> Result<StoredSecret, ClientError> {
        generated_result(
            self.generated
                .set_secret_access(application, name, access)
                .await,
        )
        .await
        .map(|response| response.data)
    }

    /// Deletes a stored secret no environment uses, and its Docker versions.
    /// # Errors
    /// Returns version/reference conflicts or transport/storage errors.
    pub async fn delete_stored_secret(
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

    /// Generates a new version of a generated secret from its declaration; a
    /// later deployment uses it.
    /// # Errors
    /// Returns absence, version conflicts or transport/storage errors.
    pub async fn regenerate_secret(
        &self,
        environment: &str,
        name: &str,
        generation: i64,
    ) -> Result<SecretMetadata, ClientError> {
        generated_result(
            self.generated
                .regenerate_environment_secret(environment, name, generation)
                .await,
        )
        .await
        .map(|response| response.data)
    }

    /// Deletes an unreferenced generated secret and its Docker versions; a
    /// later deployment generates a new value.
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
