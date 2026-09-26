//! Errors encountered while reconciling application operations.

/// Sanitized failure returned while executing a durable operation.
#[derive(Debug, thiserror::Error)]
pub enum OperationError {
    /// Docker failure with its complete diagnostic cause chain.
    #[error("{}", .0.operation_classification())]
    Docker(#[source] crate::docker::DockerError),
    /// Durable operation failure with its storage cause.
    #[error("operation journal is unavailable")]
    Journal(#[source] crate::store::StoreError),
    /// Logical names whose values must be replaced before a new deployment.
    #[error(
        "secret values unavailable: {names}; supply replacement values and start a new deployment"
    )]
    SecretUnavailable {
        /// Logical names only, never values.
        names: String,
    },
    /// Repository input could not be located or decoded.
    #[error("{}", if *.not_found { "manifest not found" } else { "repository manifest is invalid or its application name does not match" })]
    ManifestInput {
        /// Whether the input was absent rather than invalid.
        not_found: bool,
        /// Internal I/O, validation, or checkout diagnostic.
        #[source]
        source: anyhow::Error,
    },
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
    /// Git source preparation failed before rollout.
    #[error("Git source build failed")]
    GitBuildFailed(#[source] anyhow::Error),
    /// A Docker request failed while performing the described operation.
    #[error("Docker request failed while {0}")]
    DockerRequestFailed(&'static str),
    /// A local runtime value failed validation before a Docker request was made.
    #[error("Docker request validation failed while {0}")]
    ValidationFailed(&'static str),
    /// The configured repository manifest is missing.
    #[error("manifest not found")]
    ManifestNotFound,
    /// Repository access failed before reading its manifest.
    #[error("could not fetch the manifest repository")]
    ManifestFetchFailed(#[source] anyhow::Error),
    /// The fetched manifest is invalid or selects another application.
    #[error("repository manifest is invalid or its application name does not match")]
    ManifestInvalid,
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
    pub fn code(&self) -> &'static str {
        match self {
            Self::Docker(error) => error.operation_classification().code(),
            Self::Journal(_) => "journal_unavailable",
            Self::SecretUnavailable { .. } => "secret_unavailable",
            Self::ManifestInput {
                not_found: true, ..
            }
            | Self::ManifestNotFound => "manifest_not_found",
            Self::ManifestInput {
                not_found: false, ..
            }
            | Self::ManifestInvalid => "manifest_invalid",
            Self::GitBuildFailed(_) => "git_build_failed",
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
            Self::ManifestFetchFailed(_) => "manifest_fetch_failed",
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
        Self::Docker(error)
    }
}

impl crate::docker::DockerError {
    /// Projects a safe public classification without consuming the cause.
    fn operation_classification(&self) -> OperationError {
        use OperationError as Failure;
        match self {
            crate::docker::DockerError::OwnershipConflict => Failure::OwnershipConflict,
            crate::docker::DockerError::ConfigurationConflict => {
                Failure::DockerConfigurationConflict
            }
            crate::docker::DockerError::Validation(operation) => {
                Failure::ValidationFailed(operation)
            }
            crate::docker::DockerError::NotManager => Failure::SwarmManagerUnavailable,
            crate::docker::DockerError::IncompatibleSwarm => Failure::SwarmTopologyUnsupported,
            crate::docker::DockerError::Unavailable(operation)
            | crate::docker::DockerError::UnavailableSource { operation, .. } => {
                Failure::DockerUnavailable(operation)
            }
            crate::docker::DockerError::ImageResolutionSource {
                operation,
                source:
                    bollard::errors::Error::DockerResponseServerError {
                        status_code: 400..=407 | 409..=428 | 430..=499,
                        ..
                    },
            } => Failure::ImageResolutionRejected(operation),
            crate::docker::DockerError::ImageResolution(operation)
            | crate::docker::DockerError::ImageResolutionSource { operation, .. } => {
                Failure::ImageResolutionFailed(operation)
            }
            crate::docker::DockerError::Request(operation)
            | crate::docker::DockerError::RequestSource { operation, .. } => {
                Failure::DockerRequestFailed(operation)
            }
        }
    }
}

impl From<crate::store::StoreError> for OperationError {
    fn from(error: crate::store::StoreError) -> Self {
        match error {
            crate::store::StoreError::SecretUnavailable { names } => {
                Self::SecretUnavailable { names }
            }
            other => Self::Journal(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OperationError;
    use crate::docker::DockerError;
    use std::error::Error;

    #[test]
    fn operation_errors_retain_causes_without_exposing_them_publicly() {
        let diagnostic = "token=internal-secret";
        let error = OperationError::from(DockerError::RequestSource {
            operation: "inspect service",
            source: Box::new(std::io::Error::other(diagnostic)),
        });
        assert_eq!(error.code(), "docker_request_failed");
        assert!(!error.message().contains(diagnostic));
        let cause = error.source().unwrap().source().unwrap();
        assert!(cause.is::<std::io::Error>());
        assert_eq!(cause.to_string(), diagnostic);

        let journal = OperationError::from(crate::store::StoreError::DatabaseSource(
            sqlx::Error::PoolClosed,
        ));
        assert_eq!(journal.code(), "journal_unavailable");
        assert!(
            journal
                .source()
                .unwrap()
                .source()
                .unwrap()
                .is::<sqlx::Error>()
        );

        let git = OperationError::GitBuildFailed(anyhow::anyhow!(diagnostic));
        assert_eq!(git.code(), "git_build_failed");
        assert_eq!(git.source().unwrap().to_string(), diagnostic);
        assert!(!git.message().contains(diagnostic));
    }

    #[test]
    fn registry_failures_keep_classification_and_original_status() {
        for (status_code, expected) in [
            (401, "image_resolution_rejected"),
            (408, "image_resolution_failed"),
            (429, "image_resolution_failed"),
            (500, "image_resolution_failed"),
        ] {
            let error = OperationError::from(DockerError::ImageResolutionSource {
                operation: "pull image",
                source: bollard::errors::Error::DockerResponseServerError {
                    status_code,
                    message: "internal registry diagnostic".into(),
                },
            });
            assert_eq!(error.code(), expected);
            assert!(
                error
                    .source()
                    .unwrap()
                    .source()
                    .unwrap()
                    .is::<bollard::errors::Error>()
            );
            assert!(!error.message().contains("internal registry diagnostic"));
        }
    }
}
