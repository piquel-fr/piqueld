//! Errors encountered while reconciling application operations.

/// Sanitized failure returned while executing a durable operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum OperationError {
    /// The durable operation journal could not be read or updated.
    #[error("operation journal is unavailable")]
    JournalUnavailable,
    /// Operation execution was cancelled.
    #[error("operation was cancelled")]
    Cancelled,
    /// A newer application request made this operation obsolete.
    #[error("operation was superseded by a newer application request")]
    Superseded,
    /// A runtime resource is not safely owned by the application.
    #[error("a Docker resource is not safely owned by this application")]
    OwnershipConflict,
    /// An immutable Docker resource differs from the desired configuration.
    #[error("Docker resource configuration cannot be reconciled safely in place")]
    DockerConfigurationConflict,
    /// Docker is not an active Swarm manager.
    #[error("Docker is not an active Swarm manager")]
    SwarmManagerUnavailable,
    /// The Docker Swarm topology is unsupported.
    #[error("Docker Swarm must contain exactly one manager node")]
    SwarmTopologyUnsupported,
    /// Docker Engine is unavailable while performing the described operation.
    #[error("Docker Engine is unavailable while {0}")]
    DockerUnavailable(&'static str),
    /// An image could not be resolved while performing the described operation.
    #[error("container image could not be resolved to a digest while {0}")]
    ImageResolutionFailed(&'static str),
    /// The registry rejected the requested image or credentials.
    #[error("container image was rejected by the registry while {0}")]
    ImageResolutionRejected(&'static str),
    /// A Docker request failed while performing the described operation.
    #[error("Docker request failed while {0}")]
    DockerRequestFailed(&'static str),
    /// A local runtime value failed validation before a Docker request was made.
    #[error("Docker request validation failed while {0}")]
    ValidationFailed(&'static str),
    /// A service update failed in Docker.
    #[error("service update paused after task failure; the previous healthy task is retained")]
    ServiceUpdateFailed,
    /// The runtime plan is blocked by a diagnostic without a specific mapping.
    #[error("runtime plan is blocked by {0}")]
    PlanBlocked(&'static str),
    /// A service did not converge before its deadline.
    #[error("service did not converge before the deadline")]
    ConvergenceTimeout,
}

impl OperationError {
    /// Returns the stable machine-readable failure code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::JournalUnavailable => "journal_unavailable",
            Self::Cancelled => "cancelled",
            Self::Superseded => "superseded",
            Self::OwnershipConflict => "ownership_conflict",
            Self::DockerConfigurationConflict => "docker_configuration_conflict",
            Self::SwarmManagerUnavailable => "swarm_manager_unavailable",
            Self::SwarmTopologyUnsupported => "swarm_topology_unsupported",
            Self::DockerUnavailable(_) => "docker_unavailable",
            Self::ImageResolutionFailed(_) => "image_resolution_failed",
            Self::ImageResolutionRejected(_) => "image_resolution_rejected",
            Self::DockerRequestFailed(_) => "docker_request_failed",
            Self::ValidationFailed(_) => "validation_failed",
            Self::ServiceUpdateFailed => "service_update_failed",
            Self::PlanBlocked(_) => "plan_blocked",
            Self::ConvergenceTimeout => "convergence_timeout",
        }
    }

    /// Formats the stable, sanitized public failure message.
    #[must_use]
    pub fn message(&self) -> String {
        format!("{self}")
    }
}

impl From<crate::docker::DockerError> for OperationError {
    fn from(error: crate::docker::DockerError) -> Self {
        tracing::warn!(error=?error,"Docker execution error");
        match error {
            crate::docker::DockerError::OwnershipConflict => Self::OwnershipConflict,
            crate::docker::DockerError::ConfigurationConflict => Self::DockerConfigurationConflict,
            crate::docker::DockerError::Validation(operation) => Self::ValidationFailed(operation),
            crate::docker::DockerError::NotManager => Self::SwarmManagerUnavailable,
            crate::docker::DockerError::IncompatibleSwarm => Self::SwarmTopologyUnsupported,
            crate::docker::DockerError::Unavailable(operation)
            | crate::docker::DockerError::UnavailableSource { operation, .. } => {
                Self::DockerUnavailable(operation)
            }
            crate::docker::DockerError::ImageResolutionSource {
                operation,
                ref source,
            } if matches!(source,bollard::errors::Error::DockerResponseServerError {status_code:400..=407 | 409..=428 | 430..=499,..}) => {
                Self::ImageResolutionRejected(operation)
            }
            crate::docker::DockerError::ImageResolution(operation)
            | crate::docker::DockerError::ImageResolutionSource { operation, .. } => {
                Self::ImageResolutionFailed(operation)
            }
            crate::docker::DockerError::Request(operation)
            | crate::docker::DockerError::RequestSource { operation, .. }
            | crate::docker::DockerError::RequestDiagnostic { operation, .. } => {
                Self::DockerRequestFailed(operation)
            }
        }
    }
}

impl From<crate::store::StoreError> for OperationError {
    fn from(error: crate::store::StoreError) -> Self {
        tracing::error!(error = ?error, "could not read or update reconciliation state");
        Self::JournalUnavailable
    }
}
