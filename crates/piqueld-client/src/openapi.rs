//! Generated OpenAPI document retrieval.

use serde_json::Value;

use crate::{
    Client, ClientError,
    client::{GeneratedCall, collect_byte_stream},
};

impl Client {
    /// Fetches the generated `OpenAPI` document.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn openapi(&self) -> Result<Value, ClientError> {
        let response = self.generated.open_api_document().payload().await?;
        let payload = collect_byte_stream(response).await?;
        serde_json::from_slice(&payload).map_err(|source| ClientError::Decode { source })
    }
}
