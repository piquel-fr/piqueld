use crate::generated;

use crate::{Client, ClientError, Operation};

impl Client {
    /// Fetches an asynchronous operation by identifier.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn operation(&self, id: &str) -> Result<Operation, ClientError> {
        generated::GetOperation { id: id.into() }
            .send(self)
            .await
            .map(|response| response.data)
    }
}
