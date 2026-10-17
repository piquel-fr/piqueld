//! Asynchronous operation inspection.
use crate::{Client, ClientError, Operation, client::EnvelopedCall};

impl Client {
    /// Fetches an asynchronous operation by identifier.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn operation(&self, id: &str) -> Result<Operation, ClientError> {
        self.generated.get_operation(id).data().await
    }
}
