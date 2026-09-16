pub use piqueld_core::api::{DependencyStatus, ReadinessStatus, SystemStatus};

use crate::{
    Client, ClientError,
    client::{generated_error, generated_result},
};

impl Client {
    /// Fetches control-plane status.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn system_status(&self) -> Result<SystemStatus, ClientError> {
        generated_result(self.generated.system_status().await)
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
        generated_result(self.generated.system_configuration().await)
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
        match self.generated.system_readiness().await {
            Ok(response) | Err(progenitor_client::Error::ErrorResponse(response)) => {
                Ok(response.into_inner().data)
            }
            Err(error) => Err(generated_error(error).await),
        }
    }
}
