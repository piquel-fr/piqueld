use crate::{Client, ClientError, Envelope, SecretMetadata, client::path_segment};
use http::Method;
impl Client {
    /// Lists metadata without retrieving secret values.
    /// # Errors
    /// Returns transport, decoding or API errors.
    pub async fn secrets(&self, application: &str) -> Result<Vec<SecretMetadata>, ClientError> {
        self.send::<_, ()>(
            Method::GET,
            &format!(
                "{}/applications/{}/secrets",
                crate::API_PREFIX,
                path_segment(application)
            ),
            None,
            &[],
        )
        .await
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
        let path = format!(
            "{}/applications/{}/secrets/{}",
            crate::API_PREFIX,
            path_segment(application),
            path_segment(name)
        );
        let generation = generation.to_string();
        let (status, body) = self
            .exchange(
                Method::PUT,
                &path,
                value,
                &[
                    ("content-type", "application/octet-stream"),
                    ("x-expected-generation", &generation),
                ],
            )
            .await?;
        if !status.is_success() {
            return Err(crate::client::api_error(status, &body));
        }
        serde_json::from_slice::<Envelope<SecretMetadata>>(&body)
            .map(|v| v.data)
            .map_err(|source| ClientError::Decode { source })
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
        self.send::<bool, ()>(
            Method::DELETE,
            &format!(
                "{}/applications/{}/secrets/{}",
                crate::API_PREFIX,
                path_segment(application),
                path_segment(name)
            ),
            None,
            &[("x-expected-generation", &generation.to_string())],
        )
        .await
        .map(|_| ())
    }
}
