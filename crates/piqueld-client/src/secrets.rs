//! Secret metadata and write-only secret values: generated secrets per
//! environment, and each application's store of manually set secrets.
use crate::{
    Client, ClientError, SecretAccess, SecretMetadata, StoredSecret,
    client::{EnvelopedCall, GeneratedCall},
};
impl Client {
    /// Recovers from a lost master key by discarding every stored and generated value.
    /// # Errors
    /// Returns an API error if the current key still works, or transport errors.
    pub async fn recover_secret_key(&self) -> Result<crate::SecretKeyRecovery, ClientError> {
        self.generated.recover_secret_key().data().await
    }

    /// Lists an environment's generated secrets without retrieving values.
    /// # Errors
    /// Returns transport, decoding or API errors.
    pub async fn secrets(&self, environment: &str) -> Result<Vec<SecretMetadata>, ClientError> {
        self.generated.environment_secrets(environment).data().await
    }

    /// Lists an application's stored secrets and their access, never values.
    /// # Errors
    /// Returns transport, decoding or API errors.
    pub async fn stored_secrets(
        &self,
        application: &str,
    ) -> Result<Vec<StoredSecret>, ClientError> {
        self.generated.application_secrets(application).data().await
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
        self.generated
            .put_application_secret(
                application,
                name,
                environments.as_deref(),
                access.map(|access| access.previews),
                generation,
                value,
            )
            .data()
            .await
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
        self.generated
            .set_secret_access(application, name, access)
            .data()
            .await
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
        self.generated
            .delete_application_secret(application, name, generation)
            .payload()
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
        self.generated
            .regenerate_environment_secret(environment, name, generation)
            .data()
            .await
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
        self.generated
            .delete_environment_secret(environment, name, generation)
            .payload()
            .await
            .map(|_| ())
    }
}
