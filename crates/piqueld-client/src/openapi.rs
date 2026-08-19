use http::Method;
use http_body_util::BodyExt;

use crate::{Client, ClientError, decode_api_error};

impl Client {
    /// Fetches the generated `OpenAPI` document.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn openapi(&self) -> Result<serde_json::Value, ClientError> {
        self.with_request_timeout(async {
            let response = self
                .raw_request::<()>(Method::GET, "/api/v1/openapi.json", None, &[])
                .await?;
            if !response.status().is_success() {
                return Err(decode_api_error(response).await);
            }
            serde_json::from_slice(
                &response
                    .into_body()
                    .collect()
                    .await
                    .map_err(ClientError::transport)?
                    .to_bytes(),
            )
            .map_err(|_| ClientError::Decode)
        })
        .await
    }
}
