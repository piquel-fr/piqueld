//! Typed asynchronous client and transport contracts for the versioned piqueld API.

/// Application CRUD and planning endpoints.
pub mod applications;
/// Source-build status and log endpoints.
pub mod builds;
#[cfg(not(target_arch = "wasm32"))]
mod openapi;
/// Operation inspection endpoints.
pub mod operations;
/// Logical-secret metadata and transfer endpoints.
pub mod secrets;
/// Control-plane status endpoints.
pub mod system;
/// State and application transfer endpoints.
pub mod transfer;

pub use applications::{
    AcceptedOperation, ApplicationDetailView, ApplicationLogsOptions, ApplicationStatusView,
    ApplicationView, ContainerLogView, CreateApplicationRequest, DeleteApplicationRequest,
    DiagnosticView, ExpectedGeneration, ListApplicationsOptions, ObservedApplicationView,
    ObservedServiceView, PlanApplicationRequest, PlanView, ReplaceApplicationRequest,
    ReplacePlanRequest, ServiceStatusView,
};
pub use builds::{BuildLogView, BuildView};
pub use operations::{OperationStepView, OperationView};
pub use piqueld_core::manifest::Source;
pub use piqueld_core::{ValidatedApplication, ValidationErrors};
pub use secrets::{ListSecretsOptions, SecretMetadata, SecretReferenceView};
pub use system::{SystemCapabilities, SystemStatus};
pub use transfer::{
    ImportDependencyReport, MAX_STATE_ARCHIVE_BYTES, PrepareStateImportRequest, StateExportMode,
    StateImportConfirmation, StateImportResult,
};

use http::{Method, StatusCode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{fmt, future::Future, sync::Arc, time::Duration};
use thiserror::Error;
use utoipa::ToSchema;

#[cfg(not(target_arch = "wasm32"))]
use bytes::Buf;
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
#[cfg(not(target_arch = "wasm32"))]
use zeroize::Zeroize;

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

#[derive(Clone, Debug, Deserialize, Serialize)]
/// One parsed server-sent event.
pub struct SseEvent {
    /// Event identifier, when supplied by the server.
    pub id: Option<String>,
    /// Event type, when supplied by the server.
    pub event: Option<String>,
    /// Event data payload.
    pub data: String,
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

#[derive(Clone)]
/// Configured asynchronous API client.
pub struct Client {
    #[cfg(not(target_arch = "wasm32"))]
    endpoint: Endpoint,
    timeout: Duration,
    #[cfg(not(target_arch = "wasm32"))]
    bearer_token: Option<Arc<BearerToken>>,
}

#[cfg(not(target_arch = "wasm32"))]
struct BearerToken(String);

#[cfg(not(target_arch = "wasm32"))]
struct ZeroizingBody {
    bytes: Vec<u8>,
    position: usize,
}

#[cfg(not(target_arch = "wasm32"))]
impl ZeroizingBody {
    fn new(bytes: Vec<u8>) -> Self {
        Self { bytes, position: 0 }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Buf for ZeroizingBody {
    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.position)
    }

    fn chunk(&self) -> &[u8] {
        &self.bytes[self.position..]
    }

    fn advance(&mut self, count: usize) {
        self.position = self.position.saturating_add(count).min(self.bytes.len());
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for ZeroizingBody {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for BearerToken {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("Client");
        #[cfg(not(target_arch = "wasm32"))]
        debug.field("endpoint", &self.endpoint);
        debug.field("timeout", &self.timeout);
        #[cfg(not(target_arch = "wasm32"))]
        debug.field(
            "bearer_token",
            &self.bearer_token.as_ref().map(|_| "[REDACTED]"),
        );
        debug.finish()
    }
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
    /// Returns [`ClientError::Endpoint`] when `base_url` is not an HTTP origin
    /// on localhost, a private network, or the Tailscale CGNAT range.
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
            Host::Ipv4(host) if is_private_or_tailnet(host.octets()) => host.to_string(),
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
            bearer_token: None,
        })
    }

    /// Creates a client for a Unix-domain socket.
    #[must_use]
    #[cfg(not(target_arch = "wasm32"))]
    pub fn unix(path: impl AsRef<Path>) -> Self {
        Self {
            endpoint: Endpoint::Unix(path.as_ref().to_owned()),
            timeout: Duration::from_secs(30),
            bearer_token: None,
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

    /// Adds an administrative bearer token to native requests.
    ///
    /// The token is omitted from debug output and zeroized when the last
    /// client clone is dropped.
    ///
    /// # Errors
    /// Returns [`ClientError::Endpoint`] when the token cannot be represented
    /// safely in an HTTP authorization header.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn with_bearer_token(mut self, token: impl Into<String>) -> Result<Self, ClientError> {
        let token = token.into();
        if token.is_empty()
            || token.len() > 4096
            || token.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(ClientError::Endpoint);
        }
        self.bearer_token = Some(Arc::new(BearerToken(token)));
        Ok(self)
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
        if let Some(token) = &self.bearer_token {
            let mut value = Vec::with_capacity(7 + token.0.len());
            value.extend_from_slice(b"Bearer ");
            value.extend_from_slice(token.0.as_bytes());
            let header_value =
                http::HeaderValue::from_bytes(&value).map_err(|_| ClientError::Endpoint)?;
            value.zeroize();
            builder = builder.header(header::AUTHORIZATION, header_value);
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = builder
            .body(Full::new(ZeroizingBody::new(bytes)))
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

    #[cfg(not(target_arch = "wasm32"))]
    async fn decode<T: DeserializeOwned>(
        &self,
        response: hyper::Response<hyper::body::Incoming>,
    ) -> Result<T, ClientError> {
        self.with_request_timeout(decode_envelope(response)).await
    }

    #[cfg(not(target_arch = "wasm32"))]
    async fn decode_error(&self, response: hyper::Response<hyper::body::Incoming>) -> ClientError {
        tokio::time::timeout(self.timeout, decode_api_error(response))
            .await
            .unwrap_or_else(|_| ClientError::transport("error response timed out"))
    }

    #[cfg(not(target_arch = "wasm32"))]
    async fn collect_body(&self, body: hyper::body::Incoming) -> Result<bytes::Bytes, ClientError> {
        self.with_request_timeout(async {
            body.collect()
                .await
                .map_err(ClientError::transport)
                .map(http_body_util::Collected::to_bytes)
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

    /// Opens one reconnectable SSE stream. The caller owns the cursor and can
    /// pass the last received event ID into a new stream after a disconnect.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn watch_events(
        &self,
        path: String,
        last_event_id: Option<&str>,
    ) -> tokio::sync::mpsc::Receiver<Result<SseEvent, ClientError>> {
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let client = self.clone();
        let last = last_event_id.map(str::to_owned);
        tokio::spawn(async move {
            let mut headers = vec![("accept", "text/event-stream")];
            if let Some(value) = last.as_deref() {
                headers.push(("last-event-id", value));
            }
            let response = match client
                .raw_request::<()>(Method::GET, &path, None, &headers)
                .await
            {
                Ok(response) if response.status().is_success() => response,
                Ok(response) => {
                    let error = client.decode_error(response).await;
                    let _ = tx.send(Err(error)).await;
                    return;
                }
                Err(error) => {
                    let _ = tx.send(Err(error)).await;
                    return;
                }
            };
            let mut body = response.into_body();
            let mut decoder = SseDecoder::default();
            loop {
                let frame = tokio::select! {
                    () = tx.closed() => return,
                    frame = body.frame() => frame,
                };
                let Some(frame) = frame else { return };
                let Ok(frame) = frame else {
                    let _ = tx
                        .send(Err(ClientError::transport("SSE response failed")))
                        .await;
                    return;
                };
                if let Ok(data) = frame.into_data() {
                    if decoder.buffer.len() + data.len() > 2 * 1_048_576 {
                        let _ = tx.send(Err(ClientError::Decode)).await;
                        return;
                    }
                    let events = match decoder.push(&data) {
                        Ok(events) => events,
                        Err(error) => {
                            let _ = tx.send(Err(error)).await;
                            return;
                        }
                    };
                    for event in events {
                        if tx.send(Ok(event)).await.is_err() {
                            return;
                        }
                    }
                }
            }
        });
        rx
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn is_private_or_tailnet(octets: [u8; 4]) -> bool {
    matches!(
        octets,
        [10, _, _, _] | [172, 16..=31, _, _] | [192, 168, _, _] | [100, 64..=127, _, _]
    )
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

#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
}

#[cfg(not(target_arch = "wasm32"))]
impl SseDecoder {
    fn push(&mut self, data: &[u8]) -> Result<Vec<SseEvent>, ClientError> {
        self.buffer.extend_from_slice(data);
        let mut events = Vec::new();
        while let Some((end, separator_len)) = sse_block_boundary(&self.buffer) {
            let block = self.buffer[..end].to_vec();
            self.buffer.drain(..end + separator_len);
            let block = std::str::from_utf8(&block).map_err(|_| ClientError::Decode)?;
            if let Some(event) = parse_sse(block) {
                events.push(event);
            }
        }
        Ok(events)
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn sse_block_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    let mut position = 0;
    while position < buffer.len() {
        let Some(first_len) = sse_line_ending_len(buffer, position) else {
            position += 1;
            continue;
        };
        let next = position + first_len;
        if let Some(second_len) = sse_line_ending_len(buffer, next) {
            return Some((position, first_len + second_len));
        }
        position = next;
    }
    None
}

#[cfg(not(target_arch = "wasm32"))]
fn sse_line_ending_len(buffer: &[u8], position: usize) -> Option<usize> {
    match buffer.get(position) {
        Some(b'\r') if buffer.get(position + 1) == Some(&b'\n') => Some(2),
        Some(b'\n' | b'\r') => Some(1),
        _ => None,
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn parse_sse(block: &str) -> Option<SseEvent> {
    let mut id = None;
    let mut event = None;
    let mut data = Vec::new();
    for line in block.split(['\r', '\n']) {
        let (field, value) = line.split_once(':').map_or((line, ""), |(field, value)| {
            (field, value.strip_prefix(' ').unwrap_or(value))
        });
        match field {
            "id" if !value.contains('\0') => id = Some(value.to_owned()),
            "event" => event = Some(value.to_owned()),
            "data" => data.push(value),
            _ => {}
        }
    }
    (!data.is_empty()).then(|| SseEvent {
        id,
        event,
        data: data.join("\n"),
    })
}

/// Returns the client crate version embedded at build time.
#[must_use]
pub const fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn sse_decoder_handles_split_crlf_frames_and_multiline_data() {
        let mut decoder = SseDecoder::default();
        assert!(
            decoder
                .push(b"id: 42\r\nevent: application\r\ndata: {\"state\":")
                .unwrap()
                .is_empty()
        );
        let events = decoder.push(b"\"ready\"}\r\ndata: next\r\n\r\n").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id.as_deref(), Some("42"));
        assert_eq!(events[0].event.as_deref(), Some("application"));
        assert_eq!(events[0].data, "{\"state\":\"ready\"}\nnext");
    }

    #[test]
    fn sse_decoder_ignores_comments_and_rejects_nul_cursors() {
        let mut decoder = SseDecoder::default();
        let events = decoder
            .push(b": keepalive\n id: ignored\nid: bad\0cursor\nevent: logs\ndata: []\n\n")
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, None);
        assert_eq!(events[0].event.as_deref(), Some("logs"));
        assert_eq!(events[0].data, "[]");
    }

    #[test]
    fn sse_decoder_reports_invalid_utf8_only_for_complete_frames() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"data: ").unwrap().is_empty());
        assert!(matches!(
            decoder.push(&[0xff, b'\n', b'\n']),
            Err(ClientError::Decode)
        ));
    }
}
