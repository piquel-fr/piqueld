use piqueld_core::observability::DiagnosticCode;

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
    /// Swarm refused to delete a network because a task still uses it, such
    /// as the attachment task of a container that just left the network,
    /// which Swarm releases within seconds.
    #[error("Docker network is still in use by a task")]
    NetworkInUse(#[source] bollard::errors::Error),
    /// Swarm is still removing a deleted network in the background: it still
    /// inspects it, but fails deleting it again with a 500 "network … not
    /// found" until it is gone, which can take more than ten seconds.
    #[error("Docker network is still being removed")]
    NetworkRemoving(#[source] bollard::errors::Error),
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

    /// The stable classification diagnostics and retry policy use.
    #[must_use]
    pub fn diagnostic_code(&self) -> DiagnosticCode {
        match self {
            Self::OwnershipConflict => DiagnosticCode::OwnershipConflict,
            Self::ConfigurationConflict => DiagnosticCode::DockerConfigurationConflict,
            Self::Validation(_) => DiagnosticCode::ValidationFailed,
            Self::NotManager => DiagnosticCode::SwarmManagerUnavailable,
            Self::IncompatibleSwarm => DiagnosticCode::SwarmTopologyUnsupported,
            Self::Unavailable(_) | Self::UnavailableSource { .. } => {
                DiagnosticCode::DockerUnavailable
            }
            Self::ImageResolutionSource { .. } if self.registry_rejected() => {
                DiagnosticCode::ImageResolutionRejected
            }
            Self::ImageResolution(_) | Self::ImageResolutionSource { .. } => {
                DiagnosticCode::ImageResolutionFailed
            }
            Self::NetworkInUse(_)
            | Self::NetworkRemoving(_)
            | Self::Request(_)
            | Self::RequestSource { .. } => DiagnosticCode::DockerRequestFailed,
        }
    }

    /// Whether the registry rejected an image resolution: a 4xx response,
    /// except 408 and 429, which are transient failures that may be retried.
    #[must_use]
    pub const fn registry_rejected(&self) -> bool {
        matches!(
            self,
            Self::ImageResolutionSource {
                source: bollard::errors::Error::DockerResponseServerError {
                    status_code: 400..=407 | 409..=428 | 430..=499,
                    ..
                },
                ..
            }
        )
    }

    /// Builds a [`DockerError::UnavailableSource`] that keeps `source` for diagnostics.
    pub fn unavailable(
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

    /// Classifies a failed deletion of a network the adapter just inspected:
    /// Swarm refuses to delete one a task still uses with a 400 "network … is
    /// in use by task …", and fails deleting one it is still removing with a
    /// 500 "network … not found".
    pub(super) fn delete_network(source: bollard::errors::Error) -> Self {
        match &source {
            bollard::errors::Error::DockerResponseServerError {
                status_code: 400,
                message,
            } if message.contains("is in use by task") => Self::NetworkInUse(source),
            bollard::errors::Error::DockerResponseServerError {
                status_code: 500,
                message,
            } if message.starts_with("network ") && message.ends_with(" not found") => {
                Self::NetworkRemoving(source)
            }
            _ => Self::request("delete network", source),
        }
    }

    /// Whether Swarm is still releasing the network this failed to delete, so
    /// the deletion is worth retrying for longer than other failures.
    #[must_use]
    pub fn releases_network(&self) -> bool {
        matches!(self, Self::NetworkInUse(_) | Self::NetworkRemoving(_))
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

#[cfg(test)]
mod tests {
    use super::DockerError;

    /// A failed network deletion with Docker's `status` and `message`.
    fn deletion(status_code: u16, message: &str) -> DockerError {
        DockerError::delete_network(bollard::errors::Error::DockerResponseServerError {
            status_code,
            message: message.into(),
        })
    }

    #[test]
    fn network_deletion_failures_swarm_still_releases_are_classified() {
        assert!(matches!(
            deletion(
                400,
                "rpc error: code = FailedPrecondition desc = network n1 is in use by task t1"
            ),
            DockerError::NetworkInUse(_)
        ));
        assert!(matches!(
            deletion(500, "network n1 not found"),
            DockerError::NetworkRemoving(_)
        ));
        for other in [
            deletion(
                400,
                "rpc error: code = FailedPrecondition desc = network n1 is in use by service s1",
            ),
            deletion(500, "is in use by task t1"),
            deletion(503, "network n1 not found"),
        ] {
            assert!(!other.releases_network(), "{other:?}");
            assert!(
                matches!(
                    other,
                    DockerError::RequestSource {
                        operation: "delete network",
                        ..
                    }
                ),
                "{other:?}"
            );
        }
    }
}
