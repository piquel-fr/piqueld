//! Daemon status and dependency probes.

use super::{ApplicationError, ApplicationService};
use piqueld_core::api::{DependencyStatus, HostConfiguration, ReadinessStatus, SystemStatus};
use std::time::Duration;

impl ApplicationService {
    /// Returns daemon identity and version information.
    #[must_use]
    pub fn system_status(&self) -> SystemStatus {
        SystemStatus {
            status: "running".into(),
            api_version: "v1".into(),
            daemon_version: env!("CARGO_PKG_VERSION").into(),
            instance_id: self.store.instance_id().to_owned(),
        }
    }

    /// Returns effective host settings without exposing mutable configuration.
    /// # Errors
    /// Returns an error if no host configuration was supplied at construction.
    pub fn configuration(&self) -> Result<&HostConfiguration, ApplicationError> {
        self.configuration
            .as_deref()
            .ok_or(ApplicationError::ConfigurationUnavailable)
    }

    /// Probes storage, Docker, and Swarm using bounded deadlines.
    pub async fn readiness(&self) -> ReadinessStatus {
        let (database, runtime) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(2), self.store.probe()),
            tokio::time::timeout(Duration::from_secs(6), self.runtime.readiness())
        );
        let database = database.is_ok_and(|result| result.is_ok());
        let (docker, swarm) = runtime.unwrap_or((false, false));
        ReadinessStatus {
            ready: database && docker && swarm,
            database: DependencyStatus::new(database, "Database is unavailable or timed out"),
            docker: DependencyStatus::new(docker, "Docker Engine is unavailable or timed out"),
            swarm: DependencyStatus::new(
                swarm,
                "A compatible single-node Swarm manager is required",
            ),
        }
    }
}
