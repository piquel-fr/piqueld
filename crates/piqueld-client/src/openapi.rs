//! Generated OpenAPI document retrieval.

use http::Method;
use serde_json::Value;

use crate::client::api_error;
use crate::{Client, ClientError};

impl Client {
    /// Fetches the generated `OpenAPI` document.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn openapi(&self) -> Result<Value, ClientError> {
        let (status, payload) = self
            .exchange(
                Method::GET,
                &format!("{}/openapi.json", crate::API_PREFIX),
                Vec::new(),
                &[],
            )
            .await?;
        if !status.is_success() {
            return Err(api_error(status, &payload));
        }
        serde_json::from_slice(&payload).map_err(|_| ClientError::Decode)
    }
}
