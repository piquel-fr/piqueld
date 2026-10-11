//! Control-plane status, configuration, and readiness.
pub use piqueld_core::api::{
    CertificateStatus, CheckStatus, DependencyStatus, DnsProviderStatus, DnsRecordState,
    DnsRecords, DnsStatus, HostConfiguration, ImageStatus, IngressStatus, PrivateIngressStatus,
    PublicIngressStatus, ReadinessStatus, RouteStatus, SnapshotLimits, SystemCheck, SystemCheckId,
    SystemChecks, SystemStatus, TailnetStatus, TelemetryDestinationStatus, TelemetryReceiver,
    TelemetryStatus,
};

use crate::{Client, ClientError, client::EnvelopedCall};

impl Client {
    /// Fetches control-plane status.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn system_status(&self) -> Result<SystemStatus, ClientError> {
        self.generated.system_status().data().await
    }

    /// Checks every DNS provider's credentials and zones now, returning the
    /// updated providers and certificates.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn refresh_dns(&self) -> Result<DnsStatus, ClientError> {
        self.generated.refresh_dns().data().await
    }

    /// Reads effective daemon configuration without changing host settings.
    /// # Errors
    /// Returns transport, API or decoding errors.
    pub async fn system_configuration(&self) -> Result<HostConfiguration, ClientError> {
        self.generated.system_configuration().data().await
    }

    /// Checks the installation's health without changing it.
    /// # Errors
    /// Returns transport, API or decoding errors.
    pub async fn system_checks(&self) -> Result<SystemChecks, ClientError> {
        self.generated.system_checks().data().await
    }

    /// Reads diagnostic readiness, including a structured 503 response.
    /// # Errors
    /// Returns transport or decoding errors.
    pub async fn system_readiness(&self) -> Result<ReadinessStatus, ClientError> {
        match self.generated.system_readiness().await {
            Ok(response) | Err(progenitor_client::Error::ErrorResponse(response)) => {
                Ok(response.into_inner().data)
            }
            Err(error) => Err(ClientError::from_generated(error).await),
        }
    }
}
