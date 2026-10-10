//! DNS across the configured providers (`[[dns.providers]]` in daemon TOML).
//!
//! piqueld calls provider APIs itself, so DNS credentials never enter a
//! container. Providers publish ACME DNS-01 TXT records and, when
//! `manage_records` is set, routes' A/AAAA/CNAME records; [`provider`] holds
//! the provider abstraction and each provider's API.
//!
//! [`Dns`] discovers each provider's zones and assigns every hostname to the
//! provider with the longest matching zone. It also waits for records to
//! propagate to a zone's nameservers.
mod propagation;
pub mod provider;
mod zones;

pub use zones::ZoneError;

use piqueld_core::{api::DnsProviderStatus, manifest::Hostname};
use provider::{DnsProvider, DnsProviderConfig, Zone};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};

/// The latest zone discovery of one provider.
#[derive(Default)]
struct Discovery {
    /// Zones from the last successful discovery; a failed refresh keeps them.
    zones: Vec<Zone>,
    /// The latest discovery failure, cleared by a success.
    error: Option<String>,
}

/// Configured providers with their discovered zones.
pub struct Dns {
    providers: Vec<DnsProviderConfig>,
    /// Shared client for provider APIs; never follows redirects or proxies.
    http: reqwest::Client,
    /// One entry per provider, in configuration order.
    discovered: RwLock<Vec<Discovery>>,
    /// When every provider's zones were last discovered, if ever. Held for a
    /// whole discovery, so discoveries never overlap and an older one cannot
    /// overwrite a newer result.
    refreshed: Mutex<Option<Instant>>,
    /// Addresses of each nameserver queried for propagation instead of each
    /// zone's NS records.
    #[cfg(test)]
    pub(crate) nameservers: Option<Vec<Vec<std::net::SocketAddr>>>,
}

impl Dns {
    /// Zones are rediscovered after this long.
    const REFRESH: Duration = Duration::from_hours(1);

    /// Creates the inert integration; zones are discovered by [`Self::refresh`].
    ///
    /// # Errors
    /// Returns HTTP client initialization failures.
    pub fn new(providers: Vec<DnsProviderConfig>) -> reqwest::Result<Self> {
        Ok(Self {
            discovered: RwLock::new(providers.iter().map(|_| Discovery::default()).collect()),
            providers,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .build()?,
            refreshed: Mutex::new(None),
            #[cfg(test)]
            nameservers: None,
        })
    }

    /// The client used for provider APIs.
    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// The configured provider at `index`.
    pub(crate) fn provider(&self, index: usize) -> &DnsProvider {
        &self.providers[index].provider
    }

    /// Whether any provider manages routes' records.
    pub(crate) fn manages_any_records(&self) -> bool {
        self.providers.iter().any(|config| config.manage_records)
    }

    /// Whether the provider at `index` manages routes' records.
    pub(crate) fn manages_records(&self, index: usize) -> bool {
        self.providers[index].manage_records
    }

    /// Rediscovers every provider's zones when they are older than an hour, or
    /// when the previous discovery failed.
    pub async fn refresh(&self) {
        let mut refreshed = self.refreshed.lock().await;
        if refreshed.is_some_and(|at| at.elapsed() < Self::REFRESH) {
            return;
        }
        self.discover_locked(&mut refreshed).await;
    }

    /// Lists every provider's zones now, which also checks their credentials.
    /// Failures are kept per provider for status, and their old zones stay in
    /// use. Credential files are only read at startup.
    pub async fn discover(&self) {
        self.discover_locked(&mut *self.refreshed.lock().await)
            .await;
    }

    /// Discovers zones while holding `refreshed`. A failure clears it, so the
    /// next [`Self::refresh`] retries instead of waiting out the hour.
    async fn discover_locked(&self, refreshed: &mut Option<Instant>) {
        let mut discovered = Vec::with_capacity(self.providers.len());
        for config in &self.providers {
            discovered.push(config.provider.zones(&self.http).await);
        }
        let mut current = self.discovered.write().await;
        let complete = discovered.iter().all(Result::is_ok);
        for (discovery, result) in current.iter_mut().zip(discovered) {
            match result {
                Ok(zones) => *discovery = Discovery { zones, error: None },
                Err(error) => {
                    tracing::warn!(error = format!("{error:#}"), "DNS zone discovery failed");
                    discovery.error = Some(format!("{:#}", anyhow::Error::from(error)));
                }
            }
        }
        *refreshed = complete.then(Instant::now);
    }

    /// The provider (by index) and zone that own `hostname`.
    ///
    /// # Errors
    /// No zone contains it, or its longest matching zone has two providers.
    pub async fn zone_for(&self, hostname: &Hostname) -> Result<(usize, Zone), ZoneError> {
        let discovered = self.discovered.read().await;
        let zones: Vec<_> = discovered.iter().map(|d| d.zones.as_slice()).collect();
        zones::owner(&zones, hostname).map(|(index, zone)| (index, zone.clone()))
    }

    /// Per-provider kind, zones and discovery health.
    pub async fn status(&self) -> Vec<DnsProviderStatus> {
        let discovered = self.discovered.read().await;
        let zones: Vec<_> = discovered.iter().map(|d| d.zones.as_slice()).collect();
        self.providers
            .iter()
            .zip(discovered.iter())
            .enumerate()
            .map(|(index, (config, discovery))| {
                let conflicts = zones::conflicts(&zones, index);
                let message = match (&discovery.error, conflicts.is_empty()) {
                    (Some(error), _) => format!("Zone discovery failed: {error}"),
                    (None, false) => format!(
                        "Zones also claimed by another provider are not used: {}",
                        conflicts.join(", ")
                    ),
                    (None, true) if discovery.zones.is_empty() => "No zones discovered yet".into(),
                    (None, true) => "Zones discovered".into(),
                };
                DnsProviderStatus {
                    kind: config.provider.kind().into(),
                    manage_records: config.manage_records,
                    zones: discovery.zones.iter().map(|z| z.name.to_string()).collect(),
                    healthy: discovery.error.is_none() && conflicts.is_empty(),
                    message,
                }
            })
            .collect()
    }
}
