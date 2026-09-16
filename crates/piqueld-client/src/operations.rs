use crate::{Client, ClientError, Operation, client::generated_result};

impl Client {
    /// Fetches an asynchronous operation by identifier.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn operation(&self, id: &str) -> Result<Operation, ClientError> {
        generated_result(self.generated.get_operation(id).await)
            .await
            .map(|response| response.data)
    }
}
