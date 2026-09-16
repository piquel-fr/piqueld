//! Generated OpenAPI document retrieval.

use crate::generated;
use serde_json::Value;

use crate::{Client, ClientError};

impl Client {
    /// Fetches the generated `OpenAPI` document.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn openapi(&self) -> Result<Value, ClientError> {
        generated::OpenApiDocument {}.send(self).await
    }
}
