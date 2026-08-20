//! Typed asynchronous client and transport contracts for the versioned piqueld API.

/// Application CRUD and planning endpoints.
pub mod applications;
mod openapi;
/// Operation inspection endpoints.
pub mod operations;
/// Control-plane status endpoints.
pub mod system;

pub use applications::{
    AcceptedOperation, ApplicationStatusView, ApplicationView, CreateApplicationRequest,
    DeleteApplicationRequest, ExpectedGeneration, ListApplicationsOptions, PlanApplicationRequest,
    PlanView, ReplaceApplicationRequest, ReplacePlanRequest,
};
pub use operations::{OperationStepView, OperationView};
pub use system::SystemStatus;

use bytes::Bytes;
use http::{Method, Request, StatusCode, header};
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    future::Future,
    path::{Path, PathBuf},
    time::Duration,
};
use thiserror::Error;
use tokio::net::{TcpStream, UnixStream};
use url::Url;
use utoipa::ToSchema;

/// Versioned prefix used by all API endpoints.
pub const API_PREFIX: &str = "/api/v1";

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
    #[error("API transport failed")]
    Transport,
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
}

impl Client {
    /// Creates a client for a plain HTTP origin.
    ///
    /// The endpoint may include a host and optional port, but must not include
    /// credentials, a path, a query, or a fragment.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::Endpoint`] when `base_url` is not a valid plain HTTP
    /// origin.
    ///
    /// # Examples
    ///
    /// ```
    /// let client = Client::tcp("http://localhost:8080").unwrap();
    /// ```
    pub fn tcp(base_url: &str) -> Result<Self, ClientError> {
        let url = Url::parse(base_url).map_err(|_| ClientError::Endpoint)?;
        if url.scheme() != "http"
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(ClientError::Endpoint);
        }
        let host = url.host_str().ok_or(ClientError::Endpoint)?.to_owned();
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

    /// Creates a client that connects through the specified Unix-domain socket.
    ///
    /// # Examples
    ///
    /// ```
    /// let client = Client::unix("/var/run/piqueld.sock");
    /// ```
    ///
    /// # Arguments
    ///
    /// * `path` — The filesystem path of the Unix-domain socket.
    #[must_use]
    pub fn unix(path: impl AsRef<Path>) -> Self {
        Self {
            endpoint: Endpoint::Unix(path.as_ref().to_owned()),
            timeout: Duration::from_secs(30),
        }
    }

    /// Overrides the default timeout applied to each request.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// let client = Client::unix("/tmp/piqueld.sock")
    ///     .with_timeout(Duration::from_secs(10));
    /// ```
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    async fn with_request_timeout<T>(
        &self,
        future: impl Future<Output = Result<T, ClientError>>,
    ) -> Result<T, ClientError> {
        tokio::time::timeout(self.timeout, future)
            .await
            .map_err(|_| ClientError::Transport)?
    }

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
                            .map_err(|_| ClientError::Transport)?,
                    );
                    let (mut sender, connection) = http1::handshake(io)
                        .await
                        .map_err(|_| ClientError::Transport)?;
                    tokio::spawn(async move {
                        let _ = connection.await;
                    });
                    sender
                        .send_request(request)
                        .await
                        .map_err(|_| ClientError::Transport)
                }
                Endpoint::Unix(path) => {
                    let io = TokioIo::new(
                        UnixStream::connect(path)
                            .await
                            .map_err(|_| ClientError::Transport)?,
                    );
                    let (mut sender, connection) = http1::handshake(io)
                        .await
                        .map_err(|_| ClientError::Transport)?;
                    tokio::spawn(async move {
                        let _ = connection.await;
                    });
                    sender
                        .send_request(request)
                        .await
                        .map_err(|_| ClientError::Transport)
                }
            }
        })
        .await
        .map_err(|_| ClientError::Transport)?
    }

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

    /// Sends a request and decodes its successful response payload.
    ///
    /// # Examples
    ///
    /// ```
    /// # async fn example(client: &Client) -> Result<(), ClientError> {
    /// let body: Option<&serde_json::Value> = None;
    /// let response: serde_json::Value = client
    ///     .send(hyper::Method::GET, "/health", body, &[])
    ///     .await?;
    /// # let _ = response;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Arguments
    ///
    /// * `path` - The API path to request.
    /// * `body` - The optional request body.
    /// * `headers` - Additional request headers.
    ///
    /// # Returns
    ///
    /// The decoded response payload.
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
}

/// Decodes a successful API response from its JSON envelope.
///
/// Non-success responses are converted to [`ClientError::Api`], while unreadable
/// response bodies and invalid JSON produce transport and decoding errors,
/// respectively.
///
/// # Examples
///
/// ```no_run
/// # async fn example(response: hyper::Response<hyper::body::Incoming>) {
/// #[derive(serde::Deserialize)]
/// struct Payload {
///     name: String,
/// }
///
/// let payload: Payload = decode_envelope(response).await.unwrap();
/// assert!(!payload.name.is_empty());
/// # }
/// ```
///
/// # Returns
///
/// The deserialized value contained in the response envelope.
async fn decode_envelope<T: DeserializeOwned>(
    response: hyper::Response<hyper::body::Incoming>,
) -> Result<T, ClientError> {
    let status = response.status();
    let payload = response
        .into_body()
        .collect()
        .await
        .map_err(|_| ClientError::Transport)?
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

/// Converts an unsuccessful HTTP response into an API client error.
///
/// The response status is preserved, and its body is decoded as structured error
/// information when possible.
///
/// # Examples
///
/// ```no_run
/// # async fn example(response: hyper::Response<hyper::body::Incoming>) {
/// let error = decode_api_error(response).await;
/// assert!(matches!(error, ClientError::Api { .. }));
/// # }
/// ```
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

/// Percent-encodes a value for use as a URI path segment.
///
/// # Examples
///
/// ```
/// assert_eq!(path_segment("hello world"), "hello%20world");
/// assert_eq!(path_segment("a/b"), "a%2Fb");
/// ```
fn path_segment(value: &str) -> String
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

/// Provides the client crate version embedded at build time.
///
/// # Examples
///
/// ```
/// assert!(!version().is_empty());
/// ```
///
/// # Returns
///
/// The client crate version.
pub const fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
