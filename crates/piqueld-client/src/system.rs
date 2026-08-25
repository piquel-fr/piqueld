use http::Method;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{Client, ClientError};

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// Current control-plane status.
pub struct SystemStatus {
    /// Machine-readable service status.
    pub status: String,
    /// Version of the exposed API.
    pub api_version: String,
    /// Version of the running daemon binary.
    pub daemon_version: String,
    /// Control-plane instance identifier.
    pub instance_id: String,
}

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
