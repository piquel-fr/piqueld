use http::Method;

use crate::{Client, ClientError, Operation, client::path_segment};

impl Client {
    /// Fetches an asynchronous operation by identifier.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn operation(&self, id: &str) -> Result<Operation, ClientError> {
        self.send::<_, ()>(
            Method::GET,
            &format!("{}/operations/{}", crate::API_PREFIX, path_segment(id)),
            None,
            &[],
        )
        .await
    }
}
