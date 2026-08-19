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

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// Capabilities exposed by the running daemon.
// These are independent feature gates in the public wire contract, so a
// boolean matrix is clearer to operators than a single mutually-exclusive
// state enum.
#[allow(clippy::struct_excessive_bools)]
pub struct SystemCapabilities {
    /// Whether durable persistence is available.
    pub persistence: bool,
    /// Whether mutable application sources can be resolved.
    pub source_resolution: bool,
    /// Whether runtime state can be observed.
    pub runtime_observation: bool,
    /// Whether runtime mutations can be executed.
    pub runtime_execution: bool,
    /// Whether logical-secret management is configured.
    pub secret_management: bool,
    /// Optional operator-facing limitation reason.
    pub reason: Option<String>,
}

impl Client {
    /// Fetches control-plane status.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn system_status(&self) -> Result<SystemStatus, ClientError> {
        self.send::<_, ()>(Method::GET, "/api/v1/system/status", None, &[])
            .await
    }

    /// Fetches daemon capabilities.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn capabilities(&self) -> Result<SystemCapabilities, ClientError> {
        self.send::<_, ()>(Method::GET, "/api/v1/system/capabilities", None, &[])
            .await
    }
}
