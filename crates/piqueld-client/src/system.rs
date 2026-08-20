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
    /// Control-plane instance identifier.
    pub instance_id: String,
}

impl Client {
    /// Fetches the control-plane status.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] if the request, response decoding, or API response handling fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(client: &Client) -> Result<(), ClientError> {
    /// let status = client.system_status().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn system_status(&self) -> Result<SystemStatus, ClientError> {
        self.send::<_, ()>(Method::GET, "/api/v1/system/status", None, &[])
            .await
    }
}
