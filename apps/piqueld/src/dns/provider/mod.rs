//! The DNS provider abstraction: [`Provider`] is one provider's API, and
//! [`DnsProvider`] the configured account that dispatches to it.
//!
//! Each provider only speaks its own API and reports [`ApiError`]s.
//! [`DnsProvider`] adds what every provider shares: the provider, operation
//! and zone of a failure ([`DnsError`]), and treating a record that is
//! already deleted as deleted.
mod cloudflare;
mod ovh;

#[cfg(test)]
pub(crate) mod challtestsrv;
#[cfg(test)]
pub(crate) mod test_provider;

pub use cloudflare::Cloudflare;
pub use ovh::{Ovh, OvhEndpoint};

use piqueld_core::manifest::Hostname;
use reqwest::StatusCode;
use serde::{Deserialize, de::DeserializeOwned};
use std::net::{Ipv4Addr, Ipv6Addr};
use thiserror::Error;

/// One provider's DNS API. Names are fully qualified and lie inside `zone`.
pub(crate) trait Provider {
    /// The `kind` used in daemon TOML.
    fn kind(&self) -> &'static str;

    /// Lists the zones this account can manage.
    async fn zones(&self, http: &reqwest::Client) -> Result<Vec<Zone>, ApiError>;

    /// Every A, AAAA, CNAME and TXT record named exactly `name`. Other types
    /// are left out.
    async fn records(
        &self,
        http: &reqwest::Client,
        zone: &Zone,
        name: &str,
    ) -> Result<Vec<Found>, ApiError>;

    /// Creates a record named `name`, or replaces record `id` of the same
    /// type, returning its ID.
    async fn upsert(
        &self,
        http: &reqwest::Client,
        zone: &Zone,
        name: &str,
        id: Option<&RecordId>,
        record: &Record,
    ) -> Result<RecordId, ApiError>;

    /// Deletes record `id`.
    async fn delete_record(
        &self,
        http: &reqwest::Client,
        zone: &Zone,
        id: &RecordId,
    ) -> Result<(), ApiError>;

    /// Whether it can store proxied CNAME records, which tunnel routes need.
    fn proxies(&self) -> bool {
        false
    }

    /// Whether changes wait for [`Self::publish`].
    fn stages_changes(&self) -> bool {
        false
    }

    /// Applies staged changes to the zone's nameservers. Providers that
    /// apply changes immediately have nothing to do.
    async fn publish(&self, _http: &reqwest::Client, _zone: &Zone) -> Result<(), ApiError> {
        Ok(())
    }
}

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
    /// Records kept in memory, for reconciliation tests.
    #[cfg(test)]
    #[serde(skip)]
    Test(test_provider::TestProvider),
}

/// One `[[dns.providers]]` entry: a provider account and whether piqueld
/// manages routes' records in its zones.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct DnsProviderConfig {
    /// The account, selected by `kind`.
    #[serde(flatten)]
    pub provider: DnsProvider,
    /// Create, update and delete routes' A/AAAA/CNAME records in this
    /// provider's zones. Off by default, so its zones stay manual.
    #[serde(default)]
    pub manage_records: bool,
}

/// A provider whose zones stay manual.
#[cfg(test)]
impl From<DnsProvider> for DnsProviderConfig {
    fn from(provider: DnsProvider) -> Self {
        Self {
            provider,
            manage_records: false,
        }
    }
}

/// The data of one DNS record, of a type piqueld reads or writes.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Record {
    /// An IPv4 address.
    A(Ipv4Addr),
    /// An IPv6 address.
    Aaaa(Ipv6Addr),
    /// An alias of another hostname.
    Cname {
        /// The hostname aliased.
        target: Hostname,
        /// Traffic goes through Cloudflare's proxy; other providers reject it.
        proxied: bool,
    },
    /// Unquoted text.
    Txt(String),
}

impl Record {
    /// An A or AAAA record to `address`.
    #[must_use]
    pub fn address(address: std::net::IpAddr) -> Self {
        match address {
            std::net::IpAddr::V4(address) => Self::A(address),
            std::net::IpAddr::V6(address) => Self::Aaaa(address),
        }
    }

    /// The record type, as written in zone files.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::A(_) => "A",
            Self::Aaaa(_) => "AAAA",
            Self::Cname { .. } => "CNAME",
            Self::Txt(_) => "TXT",
        }
    }

    /// The record's content, without TXT quoting or a trailing dot.
    fn content(&self) -> String {
        match self {
            Self::A(address) => address.to_string(),
            Self::Aaaa(address) => address.to_string(),
            Self::Cname { target, .. } => target.to_string(),
            Self::Txt(text) => text.clone(),
        }
    }

    /// TTL in seconds: short for TXT records, which ACME challenges and
    /// ownership checks read soon after writing them.
    fn ttl(&self) -> u32 {
        match self {
            Self::Txt(_) => 60,
            _ => 300,
        }
    }

    /// Parses a provider's record of type `kind`. Other types are `None`.
    /// TXT content loses its quotes, and CNAME targets their trailing dot.
    fn parse(kind: &str, content: &str, proxied: bool) -> Result<Option<Self>, ApiError> {
        let invalid = || ApiError::Record(format!("{kind} {content}"));
        Ok(Some(match kind {
            "A" => Self::A(content.parse().map_err(|_| invalid())?),
            "AAAA" => Self::Aaaa(content.parse().map_err(|_| invalid())?),
            "CNAME" => Self::Cname {
                target: Hostname::parse(content.trim_end_matches('.').to_ascii_lowercase())
                    .map_err(|_| invalid())?,
                proxied,
            },
            "TXT" => Self::Txt(
                content
                    .strip_prefix('"')
                    .and_then(|text| text.strip_suffix('"'))
                    .unwrap_or(content)
                    .into(),
            ),
            _ => return Ok(None),
        }))
    }
}

/// `A 192.0.2.1`, `CNAME (proxied) x.cfargotunnel.com` or `TXT "text"`.
impl std::fmt::Display for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cname {
                target,
                proxied: true,
            } => write!(f, "CNAME (proxied) {target}"),
            Self::Txt(text) => write!(f, "TXT {text:?}"),
            record => write!(f, "{} {}", record.kind(), record.content()),
        }
    }
}

/// A record found at a name, with the handle that updates or deletes it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Found {
    /// The provider's handle.
    pub id: RecordId,
    /// The record's data.
    pub record: Record,
    /// Cloudflare proxies it, which for A/AAAA records replaces their
    /// addresses with Cloudflare's in public answers.
    pub proxied: bool,
}

impl Found {
    /// The record, unless its proxying differs from what it says: a proxied
    /// A/AAAA record answers with other addresses.
    #[must_use]
    pub fn exact(&self) -> Option<&Record> {
        let proxied = matches!(self.record, Record::Cname { proxied: true, .. });
        (self.proxied == proxied).then_some(&self.record)
    }
}

/// A zone hosted by one provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Zone {
    /// Apex name, such as `example.com`.
    pub name: Hostname,
    /// The provider's handle: a Cloudflare zone ID, or the name for OVH.
    pub(super) id: String,
}

impl Zone {
    /// `name` relative to the zone, as OVH names records; the apex is empty.
    fn relative<'a>(&self, name: &'a str) -> &'a str {
        name.strip_suffix(self.name.as_str())
            .map_or(name, |prefix| prefix.trim_end_matches('.'))
    }
}

/// A record created through a provider, deleted by this handle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordId(pub(crate) String);

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
    /// The API returned a record of a known type whose content is invalid.
    #[error("unexpected record: {0}")]
    Record(String),
    /// The provider cannot store this record.
    #[error("{0}")]
    Unsupported(&'static str),
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

/// Calls `$call` on the [`Provider`] behind a [`DnsProvider`], bound to `$provider`.
macro_rules! dispatch {
    ($self:ident, $provider:ident => $call:expr) => {
        match $self {
            Self::Cloudflare($provider) => $call,
            Self::Ovh($provider) => $call,
            #[cfg(test)]
            Self::Challtestsrv($provider) => $call,
            #[cfg(test)]
            Self::Test($provider) => $call,
        }
    };
}

impl DnsProvider {
    /// The `kind` used in daemon TOML.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        dispatch!(self, provider => provider.kind())
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
        dispatch!(self, provider => provider.zones(http).await)
            .map_err(self.error("list zones", None))
    }

    /// Every A, AAAA, CNAME and TXT record named exactly `name`, which is
    /// fully qualified and lies inside `zone`. Other types are left out.
    ///
    /// # Errors
    /// Returns the provider's API failure.
    pub async fn records(
        &self,
        http: &reqwest::Client,
        zone: &Zone,
        name: &str,
    ) -> Result<Vec<Found>, DnsError> {
        dispatch!(self, provider => provider.records(http, zone, name).await)
            .map_err(self.error("list records", Some(zone)))
    }

    /// Creates a record named `name`, which is fully qualified and lies
    /// inside `zone`, or replaces record `id` of the same type. It takes
    /// effect once the zone is published, so the caller holds its ID even
    /// when publishing fails.
    ///
    /// # Errors
    /// Returns the provider's API failure, or [`ApiError::Unsupported`] for a
    /// proxied CNAME outside Cloudflare.
    pub async fn upsert(
        &self,
        http: &reqwest::Client,
        zone: &Zone,
        name: &str,
        id: Option<&RecordId>,
        record: &Record,
    ) -> Result<RecordId, DnsError> {
        dispatch!(self, provider => provider.upsert(http, zone, name, id, record).await)
            .map_err(self.error("write record", Some(zone)))
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
        dispatch!(self, provider => provider.delete_record(http, zone, id).await)
            .or_else(|error| match error {
                ApiError::Status {
                    status: StatusCode::NOT_FOUND,
                    ..
                } => Ok(()),
                error => Err(error),
            })
            .map_err(self.error("delete record", Some(zone)))
    }

    /// Whether it can store proxied CNAME records; only Cloudflare can.
    #[must_use]
    pub fn proxies(&self) -> bool {
        dispatch!(self, provider => provider.proxies())
    }

    /// Whether changes wait for [`Self::publish`]; only OVH stages them.
    #[must_use]
    pub fn stages_changes(&self) -> bool {
        dispatch!(self, provider => provider.stages_changes())
    }

    /// Applies created and deleted records to the zone's nameservers. Only OVH
    /// stages changes; the other providers apply them immediately.
    ///
    /// # Errors
    /// Returns the provider's API failure.
    pub async fn publish(&self, http: &reqwest::Client, zone: &Zone) -> Result<(), DnsError> {
        dispatch!(self, provider => provider.publish(http, zone).await)
            .map_err(self.error("publish zone", Some(zone)))
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
