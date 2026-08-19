//! Typed asynchronous client and transport contracts for the versioned piqueld API.

/// Application CRUD and planning endpoints.
pub mod applications;
#[cfg(not(target_arch = "wasm32"))]
mod openapi;
/// Operation inspection endpoints.
pub mod operations;
/// Logical-secret metadata and transfer endpoints.
pub mod secrets;
/// Control-plane status endpoints.
pub mod system;

pub use applications::{
    AcceptedOperation, ApplicationDetailView, ApplicationStatusView, ApplicationView,
    CreateApplicationRequest, DeleteApplicationRequest, DiagnosticView, ExpectedGeneration,
    ListApplicationsOptions, ObservedApplicationView, ObservedServiceView, PlanApplicationRequest,
    PlanView, ReplaceApplicationRequest, ReplacePlanRequest,
};
pub use operations::{OperationStepView, OperationView};
pub use piqueld_core::manifest::Source;
pub use piqueld_core::{ValidatedApplication, ValidationErrors};
pub use secrets::{ListSecretsOptions, SecretMetadata, SecretReferenceView};
pub use system::SystemStatus;

use http::{Method, StatusCode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{fmt, future::Future, time::Duration};
use thiserror::Error;
use utoipa::ToSchema;

#[cfg(not(target_arch = "wasm32"))]
use bytes::Bytes;
#[cfg(not(target_arch = "wasm32"))]
use http::{Request, header};
#[cfg(not(target_arch = "wasm32"))]
use http_body_util::{BodyExt, Full};
#[cfg(not(target_arch = "wasm32"))]
use hyper::client::conn::http1;
#[cfg(not(target_arch = "wasm32"))]
use hyper_util::rt::TokioIo;
#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;
#[cfg(not(target_arch = "wasm32"))]
use std::path::PathBuf;
#[cfg(not(target_arch = "wasm32"))]
use tokio::net::{TcpStream, UnixStream};
#[cfg(not(target_arch = "wasm32"))]
use url::{Host, Url};
#[cfg(target_arch = "wasm32")]
use web_sys::{RequestCache, RequestCredentials, RequestMode};

/// Versioned prefix used by all API endpoints.
pub const API_PREFIX: &str = "/api/v1";

/// Validates a TOML application manifest and returns its editable name.
///
/// The daemon repeats this validation. The helper lets local CLI workflows
/// resolve a replacement target without importing the core crate directly.
///
/// # Errors
/// Returns field-level validation errors when the manifest is malformed or
/// outside the supported application schema.
pub fn application_name_from_toml(input: &str) -> Result<String, ValidationErrors> {
    piqueld_core::parse_toml(input).map(|application| application.name().to_owned())
}

/// Successful API response envelope.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Envelope<T> {
    /// Response payload.
    pub data: T,
}

/// Cursor-paginated API response.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Page<T> {
    /// Items in this page.
    pub items: Vec<T>,
    /// Cursor for the next page, when more items are available.
    pub next_cursor: Option<String>,
}

/// Structured error returned by the API.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct ErrorBody {
    /// Stable machine-readable error code.
    pub code: String,
    /// Safe human-readable error message.
    pub message: String,
    /// Optional structured error details.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub details: serde_json::Value,
    /// Server-generated request identifier.
    pub request_id: String,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug)]
enum Endpoint {
    Tcp {
        authority: String,
        host: String,
        port: u16,
    },
    Unix(PathBuf),
}

#[derive(Clone, Debug)]
/// Configured asynchronous API client.
pub struct Client {
    #[cfg(not(target_arch = "wasm32"))]
    endpoint: Endpoint,
    timeout: Duration,
}

#[derive(Debug, Error)]
/// Errors produced while making an API request.
pub enum ClientError {
    /// The endpoint URL or request could not be constructed.
    #[error("invalid API endpoint")]
    Endpoint,
    /// The connection, protocol, or request timeout failed.
    #[error("API transport failed: {message}")]
    Transport {
        /// Safe transport failure detail suitable for operator diagnostics.
        message: String,
    },
    /// The server returned a non-success response.
    #[error("API returned {status}: {error:?}")]
    Api {
        /// HTTP response status.
        status: StatusCode,
        /// Structured server error.
        error: ErrorBody,
    },
    /// The server response could not be decoded.
    #[error("API returned an invalid response")]
    Decode,
    /// A secret input path is not a private, regular, symlink-free file.
    #[error("secret input file is not a private regular file")]
    SecretFile,
}

impl Client {
    /// Creates a client for an HTTP endpoint.
    ///
    /// # Errors
    /// Returns [`ClientError::Endpoint`] when `base_url` is not a loopback HTTP origin.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn tcp(base_url: &str) -> Result<Self, ClientError> {
        let url = Url::parse(base_url).map_err(|_| ClientError::Endpoint)?;
        if url.scheme() != "http"
            || !matches!(url.path(), "" | "/")
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(ClientError::Endpoint);
        }
        let host = match url.host().ok_or(ClientError::Endpoint)? {
            Host::Domain(host) if host.eq_ignore_ascii_case("localhost") => host.to_owned(),
            Host::Ipv4(host) if host.is_loopback() => host.to_string(),
            Host::Ipv6(host) if host.is_loopback() => host.to_string(),
            _ => return Err(ClientError::Endpoint),
        };
        let port = url.port_or_known_default().ok_or(ClientError::Endpoint)?;
        let authority = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        Ok(Self {
            endpoint: Endpoint::Tcp {
                authority,
                host,
                port,
            },
            timeout: Duration::from_secs(30),
        })
    }

    /// Creates a client for a Unix-domain socket.
    #[must_use]
    #[cfg(not(target_arch = "wasm32"))]
    pub fn unix(path: impl AsRef<Path>) -> Self {
        Self {
            endpoint: Endpoint::Unix(path.as_ref().to_owned()),
            timeout: Duration::from_secs(30),
        }
    }

    /// Creates a client that fetches the daemon API from the current browser origin.
    #[cfg(target_arch = "wasm32")]
    #[must_use]
    pub fn browser() -> Self {
        Self {
            timeout: Duration::from_secs(30),
        }
    }

    /// Overrides the per-request timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    async fn with_request_timeout<T>(
        &self,
        future: impl Future<Output = Result<T, ClientError>>,
    ) -> Result<T, ClientError> {
        #[cfg(target_arch = "wasm32")]
        {
            // Browser fetches are cancelled by navigation and report network
            // failures promptly. Polling adds its own bounded cadence, so a
            // second timer dependency is unnecessary in the WASM client.
            let _ = self.timeout;
            future.await
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            tokio::time::timeout(self.timeout, future)
                .await
                .map_err(|_| ClientError::transport("request timed out"))?
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    async fn raw_request<B: Serialize>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
        headers: &[(&str, &str)],
    ) -> Result<hyper::Response<hyper::body::Incoming>, ClientError> {
        let bytes = body
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| ClientError::Decode)?
            .unwrap_or_default();
        let mut headers = headers.to_vec();
        if body.is_some() {
            headers.push(("content-type", "application/json"));
        }
        self.raw_bytes(method, path, bytes, &headers).await
    }

    #[cfg(not(target_arch = "wasm32"))]
    async fn raw_bytes(
        &self,
        method: Method,
        path: &str,
        bytes: Vec<u8>,
        headers: &[(&str, &str)],
    ) -> Result<hyper::Response<hyper::body::Incoming>, ClientError> {
        let authority = match &self.endpoint {
            Endpoint::Tcp { authority, .. } => authority.as_str(),
            Endpoint::Unix(_) => "localhost",
        };
        let mut builder = Request::builder()
            .method(method)
            .uri(format!("http://{authority}{path}"))
            .header(header::ACCEPT, "application/json");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = builder
            .body(Full::new(Bytes::from(bytes)))
            .map_err(|_| ClientError::Endpoint)?;
        let endpoint = self.endpoint.clone();
        tokio::time::timeout(self.timeout, async move {
            match endpoint {
                Endpoint::Tcp { host, port, .. } => {
                    let io = TokioIo::new(
                        TcpStream::connect((host.as_str(), port))
                            .await
                            .map_err(ClientError::transport)?,
                    );
                    let (mut sender, connection) =
                        http1::handshake(io).await.map_err(ClientError::transport)?;
                    tokio::spawn(async move {
                        let _ = connection.await;
                    });
                    sender
                        .send_request(request)
                        .await
                        .map_err(ClientError::transport)
                }
                Endpoint::Unix(path) => {
                    let io = TokioIo::new(
                        UnixStream::connect(path)
                            .await
                            .map_err(ClientError::transport)?,
                    );
                    let (mut sender, connection) =
                        http1::handshake(io).await.map_err(ClientError::transport)?;
                    tokio::spawn(async move {
                        let _ = connection.await;
                    });
                    sender
                        .send_request(request)
                        .await
                        .map_err(ClientError::transport)
                }
            }
        })
        .await
        .map_err(|_| ClientError::transport("request timed out"))?
    }

    #[cfg(not(target_arch = "wasm32"))]
    async fn send_text<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> Result<T, ClientError> {
        self.with_request_timeout(async {
            let response = self
                .raw_bytes(method, path, body.as_bytes().to_vec(), headers)
                .await?;
            decode_envelope(response).await
        })
        .await
    }

    #[cfg(target_arch = "wasm32")]
    async fn send_text<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> Result<T, ClientError> {
        self.with_request_timeout(async {
            let request = browser_request(method, path, headers)
                .header("content-type", "application/toml")
                .body(body.to_owned())
                .map_err(|_| ClientError::Decode)?;
            decode_browser_response(request.send().await.map_err(ClientError::transport)?).await
        })
        .await
    }

    #[cfg(not(target_arch = "wasm32"))]
    async fn send<T: DeserializeOwned, B: Serialize>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
        headers: &[(&str, &str)],
    ) -> Result<T, ClientError> {
        self.with_request_timeout(async {
            decode_envelope(self.raw_request(method, path, body, headers).await?).await
        })
        .await
    }

    #[cfg(target_arch = "wasm32")]
    async fn send<T: DeserializeOwned, B: Serialize>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
        headers: &[(&str, &str)],
    ) -> Result<T, ClientError> {
        self.with_request_timeout(async {
            let request = browser_request(method, path, headers);
            let response = if let Some(body) = body {
                request
                    .json(body)
                    .map_err(|_| ClientError::Decode)?
                    .send()
                    .await
            } else {
                request.send().await
            }
            .map_err(ClientError::transport)?;
            decode_browser_response(response).await
        })
        .await
    }
}

impl ClientError {
    fn transport(error: impl fmt::Display) -> Self {
        Self::Transport {
            message: error.to_string(),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
async fn decode_envelope<T: DeserializeOwned>(
    response: hyper::Response<hyper::body::Incoming>,
) -> Result<T, ClientError> {
    let status = response.status();
    let payload = response
        .into_body()
        .collect()
        .await
        .map_err(ClientError::transport)?
        .to_bytes();
    if !status.is_success() {
        return Err(ClientError::Api {
            status,
            error: error_body(&payload),
        });
    }
    serde_json::from_slice::<Envelope<T>>(&payload)
        .map(|value| value.data)
        .map_err(|_| ClientError::Decode)
}

#[cfg(target_arch = "wasm32")]
fn browser_request(
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
) -> gloo_net::http::RequestBuilder {
    let mut request = match method {
        Method::GET => gloo_net::http::Request::get(path),
        Method::POST => gloo_net::http::Request::post(path),
        Method::PUT => gloo_net::http::Request::put(path),
        Method::DELETE => gloo_net::http::Request::delete(path),
        _ => gloo_net::http::RequestBuilder::new(path).method(method),
    }
    .header("accept", "application/json")
    .cache(RequestCache::NoStore)
    .credentials(RequestCredentials::Omit)
    .mode(RequestMode::SameOrigin);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    request
}

#[cfg(target_arch = "wasm32")]
async fn decode_browser_response<T: DeserializeOwned>(
    response: gloo_net::http::Response,
) -> Result<T, ClientError> {
    let status =
        StatusCode::from_u16(response.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let payload = response.text().await.map_err(ClientError::transport)?;
    if !status.is_success() {
        return Err(ClientError::Api {
            status,
            error: error_body(payload.as_bytes()),
        });
    }
    serde_json::from_str::<Envelope<T>>(&payload)
        .map(|value| value.data)
        .map_err(|_| ClientError::Decode)
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) async fn decode_api_error(
    response: hyper::Response<hyper::body::Incoming>,
) -> ClientError {
    let status = response.status();
    let payload = response
        .into_body()
        .collect()
        .await
        .ok()
        .map(http_body_util::Collected::to_bytes)
        .unwrap_or_default();
    ClientError::Api {
        status,
        error: error_body(&payload),
    }
}

fn error_body(payload: &[u8]) -> ErrorBody {
    serde_json::from_slice(payload).unwrap_or(ErrorBody {
        code: "invalid_error_response".into(),
        message: "server returned an unreadable error".into(),
        details: serde_json::Value::Null,
        request_id: String::new(),
    })
}

fn path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

/// Returns the client crate version embedded at build time.
#[must_use]
pub const fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
