//! DNS provider integrations configured in daemon TOML (`[[dns.providers]]`).
//!
//! piqueld calls provider APIs itself, so DNS credentials never enter a
//! container. Today providers publish ACME DNS-01 TXT records; record
//! management (#194) adds methods to the same [`DnsProvider`] enum.
//!
//! [`Dns`] discovers each provider's zones and assigns every hostname to the
//! provider with the longest matching zone.
mod cloudflare;
mod ovh;
mod propagation;
mod zones;

#[cfg(test)]
pub(crate) mod challtestsrv;

pub use cloudflare::Cloudflare;
pub use ovh::{Ovh, OvhEndpoint};
pub use zones::ZoneError;

use piqueld_core::{api::DnsProviderStatus, manifest::Hostname};
use reqwest::StatusCode;
use serde::{Deserialize, de::DeserializeOwned};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};

/// One DNS provider account, selected by `kind` in daemon TOML. Credentials
/// are only accepted through `_file` settings.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum DnsProvider {
    /// Cloudflare API with a scoped token (Zone:Read and DNS:Edit).
    Cloudflare(Cloudflare),
    /// OVH API with an application key, secret and consumer key.
    Ovh(Ovh),
    /// Pebble's `challtestsrv`, which serves TXT records to a test CA.
    #[cfg(test)]
    Challtestsrv(challtestsrv::Challtestsrv),
}

/// A zone hosted by one provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Zone {
    /// Apex name, such as `example.com`.
    pub name: Hostname,
    /// The provider's handle: a Cloudflare zone ID, or the name for OVH.
    id: String,
}

/// A record created through a provider, deleted by this handle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordId(String);

/// A failed provider call. It names the provider, operation and zone, and
/// keeps the API's response; credentials are only ever sent in headers, so
/// they never appear here.
#[derive(Debug, Error)]
#[error("{provider} {operation}{}", zone.as_ref().map(|zone| format!(" in zone {zone}")).unwrap_or_default())]
pub struct DnsError {
    provider: &'static str,
    operation: &'static str,
    zone: Option<Hostname>,
    #[source]
    source: ApiError,
}

/// Why a provider API call failed.
#[derive(Debug, Error)]
pub enum ApiError {
    /// The request could not be sent or its response could not be read.
    #[error("request failed")]
    Request(#[from] reqwest::Error),
    /// The API answered with a non-success status.
    #[error("HTTP {status}: {body}")]
    Status {
        /// Response status.
        status: StatusCode,
        /// Response body, truncated to 512 characters.
        body: String,
    },
    /// The API answered with an unexpected body.
    #[error("unexpected response: {0}")]
    Decode(#[from] serde_json::Error),
}

/// Sends a request and decodes its JSON response; an empty body decodes as
/// `null`. Non-success statuses keep a bounded body for diagnostics.
async fn send<T: DeserializeOwned>(request: reqwest::RequestBuilder) -> Result<T, ApiError> {
    let response = request.send().await?;
    let status = response.status();
    let body = response.bytes().await?;
    if !status.is_success() {
        return Err(ApiError::Status {
            status,
            body: String::from_utf8_lossy(&body).chars().take(512).collect(),
        });
    }
    Ok(serde_json::from_slice(if body.is_empty() {
        b"null"
    } else {
        &body
    })?)
}

impl DnsProvider {
    /// The `kind` used in daemon TOML.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Cloudflare(_) => "cloudflare",
            Self::Ovh(_) => "ovh",
            #[cfg(test)]
            Self::Challtestsrv(_) => "challtestsrv",
        }
    }

    /// Wraps an API failure with this provider, the operation and the zone.
    fn error(
        &self,
        operation: &'static str,
        zone: Option<&Zone>,
    ) -> impl FnOnce(ApiError) -> DnsError {
        let provider = self.kind();
        let zone = zone.map(|zone| zone.name.clone());
        move |source| DnsError {
            provider,
            operation,
            zone,
            source,
        }
    }

    /// Lists the zones this account can manage.
    ///
    /// # Errors
    /// Returns the provider's API failure.
    pub async fn zones(&self, http: &reqwest::Client) -> Result<Vec<Zone>, DnsError> {
        match self {
            Self::Cloudflare(provider) => provider.zones(http).await,
            Self::Ovh(provider) => provider.zones(http).await,
            #[cfg(test)]
            Self::Challtestsrv(provider) => Ok(provider.zones()),
        }
        .map_err(self.error("list zones", None))
    }

    /// Creates a TXT record. `name` is fully qualified and lies inside `zone`.
    /// It takes effect once the zone is published, so the caller holds its ID
    /// even when publishing fails.
    ///
    /// # Errors
    /// Returns the provider's API failure.
    pub async fn create_txt(
        &self,
        http: &reqwest::Client,
        zone: &Zone,
        name: &str,
        value: &str,
    ) -> Result<RecordId, DnsError> {
        match self {
            Self::Cloudflare(provider) => provider.create_txt(http, zone, name, value).await,
            Self::Ovh(provider) => provider.create_txt(http, zone, name, value).await,
            #[cfg(test)]
            Self::Challtestsrv(provider) => provider.create_txt(http, name, value).await,
        }
        .map_err(self.error("create TXT record", Some(zone)))
    }

    /// Deletes a record this daemon created; one that is already gone counts
    /// as deleted, so a retried deletion converges. It takes effect once the
    /// zone is published.
    ///
    /// # Errors
    /// Returns the provider's API failure.
    pub async fn delete_record(
        &self,
        http: &reqwest::Client,
        zone: &Zone,
        id: &RecordId,
    ) -> Result<(), DnsError> {
        match self {
            Self::Cloudflare(provider) => provider.delete_record(http, zone, id).await,
            Self::Ovh(provider) => provider.delete_record(http, zone, id).await,
            #[cfg(test)]
            Self::Challtestsrv(provider) => provider.delete_record(http, id).await,
        }
        .or_else(|error| match error {
            ApiError::Status {
                status: StatusCode::NOT_FOUND,
                ..
            } => Ok(()),
            error => Err(error),
        })
        .map_err(self.error("delete record", Some(zone)))
    }

    /// Applies created and deleted records to the zone's nameservers. Only OVH
    /// stages changes; the other providers apply them immediately.
    ///
    /// # Errors
    /// Returns the provider's API failure.
    pub async fn publish(&self, http: &reqwest::Client, zone: &Zone) -> Result<(), DnsError> {
        match self {
            Self::Ovh(provider) => provider.refresh(http, zone).await,
            Self::Cloudflare(_) => Ok(()),
            #[cfg(test)]
            Self::Challtestsrv(_) => Ok(()),
        }
        .map_err(self.error("publish zone", Some(zone)))
    }
}

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
    providers: Vec<DnsProvider>,
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
    pub fn new(providers: Vec<DnsProvider>) -> reqwest::Result<Self> {
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
        &self.providers[index]
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
        for provider in &self.providers {
            discovered.push(provider.zones(&self.http).await);
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
            .map(|(index, (provider, discovery))| {
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
                    kind: provider.kind().into(),
                    zones: discovery.zones.iter().map(|z| z.name.to_string()).collect(),
                    healthy: discovery.error.is_none() && conflicts.is_empty(),
                    message,
                }
            })
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use axum::{body::Bytes, extract::State, http::Method, response::IntoResponse};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// One request received by [`Recorded`].
    pub(crate) struct Request {
        pub(crate) method: Method,
        /// Path with its query.
        pub(crate) path: String,
        pub(crate) authorization: Option<String>,
        headers: axum::http::HeaderMap,
        /// JSON body, or `null` when empty.
        pub(crate) body: serde_json::Value,
    }

    impl Request {
        pub(crate) fn header(&self, name: &str) -> Option<&str> {
            self.headers.get(name).and_then(|value| value.to_str().ok())
        }
    }

    type Responses = Vec<(Method, &'static str, u16, &'static str)>;
    /// Responses to replay, and the requests received so far.
    type Shared = (Arc<Responses>, Arc<Mutex<Vec<Request>>>);

    /// A local HTTP server replaying recorded provider responses by method and
    /// path, and recording what it received.
    pub(crate) struct Recorded {
        pub(crate) url: String,
        requests: Arc<Mutex<Vec<Request>>>,
        _server: tokio::task::JoinHandle<()>,
    }

    impl Recorded {
        pub(crate) async fn serve(responses: Responses) -> Self {
            let requests = Arc::new(Mutex::new(Vec::new()));
            let state = (Arc::new(responses), Arc::clone(&requests));
            let app = axum::Router::new().fallback(
                |State((responses, requests)): State<Shared>,
                 method: Method,
                 uri: axum::http::Uri,
                 headers: axum::http::HeaderMap,
                 body: Bytes| async move {
                    let path = uri.path_and_query().map_or("/", |p| p.as_str()).to_owned();
                    let response = responses
                        .iter()
                        .find(|(m, p, ..)| *m == method && *p == path)
                        .unwrap_or_else(|| panic!("unexpected request {method} {path}"));
                    requests.lock().await.push(Request {
                        authorization: headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(Into::into),
                        method,
                        path,
                        headers,
                        body: serde_json::from_slice(&body).unwrap_or_default(),
                    });
                    (
                        axum::http::StatusCode::from_u16(response.2).unwrap(),
                        [("content-type", "application/json")],
                        response.3,
                    )
                        .into_response()
                },
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                axum::serve(listener, app.with_state(state)).await.unwrap();
            });
            Self {
                url,
                requests,
                _server: server,
            }
        }

        /// Requests received so far, in order.
        pub(crate) async fn requests(&self) -> Vec<Request> {
            std::mem::take(&mut *self.requests.lock().await)
        }
    }
}
