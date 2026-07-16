use http::Method;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{Client, ClientError};

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize, ToSchema)]
pub struct SystemStatus {
    pub status: String,
    pub api_version: String,
    pub instance_id: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize, ToSchema)]
pub struct SystemCapabilities {
    pub persistence: bool,
    pub source_resolution: bool,
    pub runtime_observation: bool,
    pub runtime_execution: bool,
    pub reason: Option<String>,
}

impl Client {
    pub async fn system_status(&self) -> Result<SystemStatus, ClientError> {
        self.send::<_, ()>(Method::GET, "/api/v1/system/status", None, &[])
            .await
    }

    pub async fn capabilities(&self) -> Result<SystemCapabilities, ClientError> {
        self.send::<_, ()>(Method::GET, "/api/v1/system/capabilities", None, &[])
            .await
    }
}
