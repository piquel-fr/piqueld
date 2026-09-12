use http::Method;

pub use piqueld_core::api::SystemStatus;

use crate::{Client, ClientError};

impl Client {
    /// Fetches control-plane status.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn system_status(&self) -> Result<SystemStatus, ClientError> {
        self.send::<_, ()>(
            Method::GET,
            &format!("{}/system/status", crate::API_PREFIX),
            None,
            &[],
        )
        .await
    }
}
