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
}

impl DockerError {
    /// Creates an error indicating that Docker was unavailable during an operation.
    
    ///
    
    /// The underlying Docker error is retained for diagnostics.
    
    ///
    
    /// # Examples
    
    ///
    
    /// ```
    
    /// let source = bollard::errors::Error::DockerResponseServerError {
    
    ///     status_code: 503,
    
    ///     message: "Docker unavailable".into(),
    
    /// };
    
    /// let error = DockerError::unavailable("listing containers", source);
    
    /// ```
    pub(super) fn unavailable(operation: &'static str, source: bollard::errors::Error) -> Self {
        Self::UnavailableSource { operation, source }
    }

    /// Creates an image-resolution error that preserves the failed operation and underlying Docker error.
    ///
    /// # Examples
    ///
    /// ```
    /// let source = bollard::errors::Error::DockerResponseServerError {
    ///     status_code: 404,
    ///     message: "image not found".to_owned(),
    /// };
    /// let error = DockerError::image_resolution("pull image", source);
    /// assert!(matches!(error, DockerError::ImageResolutionSource { .. }));
    /// ```
    ///
    /// `operation` identifies the Docker operation that failed.
    pub(super) fn image_resolution(
        operation: &'static str,
        source: bollard::errors::Error,
    ) -> Self {
        Self::ImageResolutionSource { operation, source }
    }

    /// Creates a request error while preserving the operation description and underlying Docker error.
    ///
    /// # Examples
    ///
    /// ```
    /// let source = bollard::errors::Error::DockerResponseServerError {
    ///     status_code: 500,
    ///     message: "request failed".to_owned(),
    /// };
    /// let error = DockerError::request("pull image", source);
    ///
    /// assert!(matches!(
    ///     error,
    ///     DockerError::RequestSource {
    ///         operation: "pull image",
    ///         ..
    ///     }
    /// ));
    /// ```
    ///
    /// # Returns
    ///
    /// A request error containing the operation description and underlying Docker error.
    pub(super) fn request(operation: &'static str, source: bollard::errors::Error) -> Self {
        Self::RequestSource { operation, source }
    }
}

impl From<DockerError> for OperationError {
    /// Converts a Docker-specific error into its corresponding operation error.
    ///
    /// # Examples
    ///
    /// ```
    /// let error: OperationError = DockerError::OwnershipConflict.into();
    /// assert!(matches!(error, OperationError::OwnershipConflict));
    /// ```
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
            DockerError::Request(operation) | DockerError::RequestSource { operation, .. } => {
                Self::DockerRequestFailed(operation)
            }
        }
    }
}
