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

impl Client {
    /// Reads effective daemon configuration without changing host settings.
    /// # Errors
    /// Returns transport, API or decoding errors.
    pub async fn system_configuration(
        &self,
    ) -> Result<piqueld_core::api::HostConfiguration, ClientError> {
        self.send::<_, ()>(
            Method::GET,
            &format!("{}/system/configuration", crate::API_PREFIX),
            None,
            &[],
        )
        .await
    }
}

impl Client {
    /// Reads diagnostic readiness, including a structured 503 response.
    /// # Errors
    /// Returns transport or decoding errors.
    pub async fn system_readiness(
        &self,
    ) -> Result<piqueld_core::api::ReadinessStatus, ClientError> {
        let (status, bytes) = self
            .exchange(
                Method::GET,
                &format!("{}/system/readiness", crate::API_PREFIX),
                Vec::new(),
                &[],
            )
            .await?;
        if !status.is_success() && status != http::StatusCode::SERVICE_UNAVAILABLE {
            return Err(crate::client::api_error(status, &bytes));
        }
        serde_json::from_slice::<crate::Envelope<piqueld_core::api::ReadinessStatus>>(&bytes)
            .map(|envelope| envelope.data)
            .map_err(|source| ClientError::Decode { source })
    }
}
