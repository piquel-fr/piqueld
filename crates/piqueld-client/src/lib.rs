//! Typed asynchronous client and transport contracts for the versioned piqueld API.
#![allow(missing_docs)]
#![allow(clippy::missing_errors_doc, clippy::struct_excessive_bools)]

pub mod applications;
mod openapi;
pub mod operations;
pub mod system;

pub use applications::{
    AcceptedOperation, ApplicationStatusView, ApplicationView, CreateApplicationRequest,
    DeleteApplicationRequest, ExpectedGeneration, ListApplicationsOptions, PlanApplicationRequest,
    PlanView, ReplaceApplicationRequest, ReplacePlanRequest,
};
pub use operations::{OperationStepView, OperationView};
pub use system::{SystemCapabilities, SystemStatus};

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

pub const API_PREFIX: &str = "/api/v1";

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Envelope<T> {
    pub data: T,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub details: serde_json::Value,
    pub request_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SseEvent {
    pub id: Option<String>,
    pub event: Option<String>,
    pub data: String,
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
pub struct Client {
    endpoint: Endpoint,
    timeout: Duration,
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("invalid API endpoint")]
    Endpoint,
    #[error("API transport failed")]
    Transport,
    #[error("API returned {status}: {error:?}")]
    Api {
        status: StatusCode,
        error: ErrorBody,
    },
    #[error("API returned an invalid response")]
    Decode,
}

impl Client {
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

    #[must_use]
    pub fn unix(path: impl AsRef<Path>) -> Self {
        Self {
            endpoint: Endpoint::Unix(path.as_ref().to_owned()),
            timeout: Duration::from_secs(30),
        }
    }

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

    fn watch_events(
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
                    let error = tokio::time::timeout(client.timeout, decode_api_error(response))
                        .await
                        .unwrap_or(ClientError::Transport);
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
                let Some(frame) = frame else {
                    return;
                };
                let Ok(frame) = frame else {
                    let _ = tx.send(Err(ClientError::Transport)).await;
                    return;
                };
                if let Ok(data) = frame.into_data() {
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

async fn decode_api_error(response: hyper::Response<hyper::body::Incoming>) -> ClientError {
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

#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
}

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

fn sse_line_ending_len(buffer: &[u8], position: usize) -> Option<usize> {
    match buffer.get(position) {
        Some(b'\r') if buffer.get(position + 1) == Some(&b'\n') => Some(2),
        Some(b'\n' | b'\r') => Some(1),
        _ => None,
    }
}

fn parse_sse(block: &str) -> Option<SseEvent> {
    let mut id = None;
    let mut kind = None;
    let mut data = Vec::new();
    for line in block.lines() {
        if line.starts_with(':') {
            continue;
        }
        if let Some(value) = line.strip_prefix("id:") {
            id = Some(value.trim_start().to_owned());
        } else if let Some(value) = line.strip_prefix("event:") {
            kind = Some(value.trim_start().to_owned());
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.trim_start());
        }
    }
    (!data.is_empty()).then(|| SseEvent {
        id,
        event: kind,
        data: data.join("\n"),
    })
}

/// Returns the client crate version embedded at build time.
#[must_use]
pub const fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::SseDecoder;

    #[test]
    fn sse_decoder_preserves_utf8_split_across_frames() {
        let mut decoder = SseDecoder::default();
        assert!(
            decoder
                .push(b"event: operation\ndata: caf\xc3")
                .unwrap()
                .is_empty()
        );

        let events = decoder.push(b"\xa9\n\n").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("operation"));
        assert_eq!(events[0].data, "caf\u{e9}");

        let events = decoder.push(b"data: mixed\r\n\n").unwrap();
        assert_eq!(events[0].data, "mixed");
    }
}
