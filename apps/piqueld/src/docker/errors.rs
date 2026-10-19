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
        source: Box<dyn std::error::Error + Send + Sync>,
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
    /// A Docker request failed, retaining its underlying cause for diagnostics.
    #[error("Docker request failed while {operation}")]
    RequestSource {
        /// The operation attempted against Docker.
        operation: &'static str,
        /// The internal failure; public formatting excludes this detail.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl DockerError {
    /// Whether retrying within the current attempt cannot help: failures
    /// diagnostic policy never retries, and Swarm topology failures, which
    /// don't recover within a request's short backoff; the operation's own
    /// backoff retries those.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::NotManager | Self::IncompatibleSwarm)
            || !self.diagnostic_code().retryable()
    }

    /// Builds a [`DockerError::UnavailableSource`] that keeps `source` for diagnostics.
    pub(crate) fn unavailable(
        operation: &'static str,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::UnavailableSource {
            operation,
            source: Box::new(source),
        }
    }

    /// Builds an [`DockerError::ImageResolutionSource`] from a Bollard failure.
    pub(super) fn image_resolution(
        operation: &'static str,
        source: bollard::errors::Error,
    ) -> Self {
        Self::ImageResolutionSource { operation, source }
    }

    /// Builds a [`DockerError::RequestSource`] that keeps `source` for diagnostics.
    pub(super) fn request(
        operation: &'static str,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::RequestSource {
            operation,
            source: Box::new(source),
        }
    }
}

/// Classifies Bollard results at the adapter boundary.
pub(super) trait BollardResult<T> {
    /// Keeps a failure as [`DockerError::RequestSource`] labelled with `operation`.
    fn or_request(self, operation: &'static str) -> Result<T, DockerError>;

    /// Turns Docker's HTTP 404 into `Ok(None)`: the resource does not exist.
    fn found(self) -> Result<Option<T>, bollard::errors::Error>;
}

impl<T> BollardResult<T> for Result<T, bollard::errors::Error> {
    fn or_request(self, operation: &'static str) -> Result<T, DockerError> {
        self.map_err(|error| DockerError::request(operation, error))
    }

    fn found(self) -> Result<Option<T>, bollard::errors::Error> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(None),
            Err(error) => Err(error),
        }
    }
}
