//! Daemon status and dependency probes.

use super::{ApplicationError, ApplicationService};
use piqueld_core::access::Scope;
use piqueld_core::api::{
    DependencyStatus, DnsStatus, HostConfiguration, ReadinessStatus, SystemStatus,
};
use std::time::Duration;

impl ApplicationService {
    /// Returns daemon identity and version information, with the tailnet node,
    /// DNS providers and DNS-01 certificates, and previews against their
    /// limits, naming only `readable` applications.
    pub async fn system_status(&self, readable: &Scope) -> SystemStatus {
        // Status reports storage outages instead of failing, so preview
        // counts that cannot be read are absent rather than zero.
        let previews = self
            .store
            .preview_usage(readable)
            .await
            .inspect_err(|error| tracing::warn!(?error, "could not count previews"))
            .ok();
        SystemStatus {
            status: "running".into(),
            api_version: "v1".into(),
            daemon_version: env!("CARGO_PKG_VERSION").into(),
            instance_id: self.store.instance_id().to_owned(),
            tailscale: self
                .tailnet
                .as_ref()
                .map(|status| status.borrow().clone())
                .unwrap_or_default(),
            dns: match &self.ingress {
                Some(ingress) => ingress.dns_status().await,
                None => DnsStatus::default(),
            },
            previews,
        }
    }

    /// Checks every DNS provider's credentials and zones now, returning the
    /// updated providers and certificates.
    pub async fn refresh_dns(&self) -> DnsStatus {
        match &self.ingress {
            Some(ingress) => ingress.refresh_dns().await,
            None => DnsStatus::default(),
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

    /// Probes storage, Docker, and Swarm using bounded deadlines. Ingress
    /// routes are limited to the environments of `readable` applications.
    pub async fn readiness(&self, readable: &Scope) -> ReadinessStatus {
        let (database, runtime) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(2), self.store.probe()),
            tokio::time::timeout(Duration::from_secs(6), self.runtime.readiness())
        );
        let database = database.is_ok_and(|result| result.is_ok());
        let (docker, swarm) = runtime.unwrap_or((false, false));
        let mut ingress = match &self.ingress {
            Some(ingress) => ingress.status().await,
            None => piqueld_core::api::IngressStatus::default(),
        };
        // Readiness reports storage outages instead of failing, so routes
        // whose applications cannot be looked up are hidden.
        ingress.routes = self
            .store
            .readable_routes(readable, ingress.routes)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(
                    ?error,
                    "could not limit ingress routes to readable applications"
                );
                Vec::new()
            });
        ReadinessStatus {
            ingress,
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
