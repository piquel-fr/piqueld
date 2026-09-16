use crate::generated;

pub use piqueld_core::api::{DependencyStatus, ReadinessStatus, SystemStatus};

use crate::{Client, ClientError};

impl Client {
    /// Fetches control-plane status.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn system_status(&self) -> Result<SystemStatus, ClientError> {
        generated::SystemStatus {}
            .send(self)
            .await
            .map(|response| response.data)
    }
}

impl Client {
    /// Reads effective daemon configuration without changing host settings.
    /// # Errors
    /// Returns transport, API or decoding errors.
    pub async fn system_configuration(
        &self,
    ) -> Result<piqueld_core::api::HostConfiguration, ClientError> {
        generated::SystemConfiguration {}
            .send(self)
            .await
            .map(|response| response.data)
    }
}

impl Client {
    /// Reads diagnostic readiness, including a structured 503 response.
    /// # Errors
    /// Returns transport or decoding errors.
    pub async fn system_readiness(
        &self,
    ) -> Result<piqueld_core::api::ReadinessStatus, ClientError> {
        let response: crate::Envelope<ReadinessStatus> = generated::SystemReadiness {}
            .request()
            .send_json(self, &[http::StatusCode::SERVICE_UNAVAILABLE])
            .await?;
        Ok(response.data)
    }
}
