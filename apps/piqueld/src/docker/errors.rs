use super::OperationError;

#[derive(Debug, thiserror::Error)]
/// A stable Docker boundary error.
pub enum DockerError {
    /// Docker could not be reached while performing the described operation.
    #[error("Docker Engine is unavailable while {0}")]
    Unavailable(&'static str),
    /// Docker could not be reached, retaining the engine error for diagnostics.
    #[error("Docker Engine is unavailable while {operation}")]
    UnavailableSource {
        /// The operation attempted against Docker.
        operation: &'static str,
        /// The underlying engine failure.
        #[source]
        source: bollard::errors::Error,
    },
    /// The engine is reachable but is not an active Swarm manager.
    #[error("Docker Engine is not an active Swarm manager")]
    NotManager,
    /// The Swarm does not satisfy piqueld's single-node topology contract.
    #[error("Docker Swarm is not a compatible single-node cluster")]
    IncompatibleSwarm,
    /// A resource exists but is not owned by the requested application.
    #[error("Docker resource ownership conflict")]
    OwnershipConflict,
    /// An owned resource has immutable settings that cannot be repaired safely.
    #[error("Docker resource configuration cannot be reconciled in place")]
    ConfigurationConflict,
    /// A local value failed validation before a Docker request was made.
    #[error("Docker request validation failed while {0}")]
    Validation(&'static str),
    /// A requested image could not be resolved to a repository digest.
    #[error("container image could not be resolved to a digest while {0}")]
    ImageResolution(&'static str),
    /// Image resolution failed in Docker, retaining the engine error for diagnostics.
    #[error("container image could not be resolved to a digest while {operation}")]
    ImageResolutionSource {
        /// The image-resolution operation attempted against Docker.
        operation: &'static str,
        /// The underlying engine failure.
        #[source]
        source: bollard::errors::Error,
    },
    /// A Docker request failed while performing the described operation.
    #[error("Docker request failed while {0}")]
    Request(&'static str),
    /// A Docker request failed, retaining the engine error for diagnostics.
    #[error("Docker request failed while {operation}")]
    RequestSource {
        /// The operation attempted against Docker.
        operation: &'static str,
        /// The underlying engine failure.
        #[source]
        source: bollard::errors::Error,
    },
    /// A raw Engine response failed, retaining bounded diagnostic context.
    #[error("Docker request failed while {operation}")]
    RequestDiagnostic {
        /// The operation attempted against Docker.
        operation: &'static str,
        /// Bounded internal response detail.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl DockerError {
    pub(super) fn unavailable(operation: &'static str, source: bollard::errors::Error) -> Self {
        Self::UnavailableSource { operation, source }
    }

    pub(super) fn image_resolution(
        operation: &'static str,
        source: bollard::errors::Error,
    ) -> Self {
        Self::ImageResolutionSource { operation, source }
    }

    pub(super) fn request(operation: &'static str, source: bollard::errors::Error) -> Self {
        Self::RequestSource { operation, source }
    }
}

impl From<DockerError> for OperationError {
    fn from(error: DockerError) -> Self {
        match error {
            DockerError::OwnershipConflict => Self::OwnershipConflict,
            DockerError::ConfigurationConflict => Self::DockerConfigurationConflict,
            DockerError::Validation(operation) => Self::ValidationFailed(operation),
            DockerError::NotManager => Self::SwarmManagerUnavailable,
            DockerError::IncompatibleSwarm => Self::SwarmTopologyUnsupported,
            DockerError::Unavailable(operation)
            | DockerError::UnavailableSource { operation, .. } => {
                Self::DockerUnavailable(operation)
            }
            DockerError::ImageResolution(operation)
            | DockerError::ImageResolutionSource { operation, .. } => {
                Self::ImageResolutionFailed(operation)
            }
            DockerError::Request(operation)
            | DockerError::RequestSource { operation, .. }
            | DockerError::RequestDiagnostic { operation, .. } => {
                Self::DockerRequestFailed(operation)
            }
        }
    }
}
