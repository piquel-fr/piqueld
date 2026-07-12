//! Typed asynchronous client and transport contracts for the versioned piqueld API.
#![allow(missing_docs)]
#![allow(clippy::missing_errors_doc, clippy::struct_excessive_bools)]

use bytes::Bytes;
use http::{Method, Request, StatusCode, header};
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use piqueld_core::manifest::ApplicationManifest;
use piqueld_core::{NormalizedApplication, Plan};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use thiserror::Error;
use tokio::net::{TcpStream, UnixStream};
use url::Url;

pub const API_PREFIX: &str = "/api/v1";

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct Envelope<T> {
    pub data: T,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub details: serde_json::Value,
    pub request_id: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct SystemStatus {
    pub status: String,
    pub api_version: String,
    pub instance_id: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct SystemCapabilities {
    pub persistence: bool,
    pub source_resolution: bool,
    pub runtime_observation: bool,
    pub runtime_execution: bool,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct ApplicationView {
    pub application: NormalizedApplication,
    #[schemars(range(min = 1))]
    pub generation: u64,
    pub spec_hash: String,
    pub delete_intent: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateApplicationRequest {
    pub manifest: ApplicationManifest,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaceApplicationRequest {
    #[schemars(range(min = 1))]
    pub expected_generation: u64,
    pub manifest: ApplicationManifest,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanApplicationRequest {
    pub manifest: ApplicationManifest,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReplacePlanRequest {
    #[schemars(range(min = 1))]
    pub expected_generation: u64,
    pub manifest: ApplicationManifest,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedGeneration {
    #[schemars(range(min = 1))]
    pub expected_generation: u64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteApplicationRequest {
    #[schemars(range(min = 1))]
    pub expected_generation: u64,
    #[serde(default)]
    pub force: bool,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct AcceptedOperation {
    pub operation_id: String,
    pub application_id: String,
    #[schemars(range(min = 1))]
    pub generation: u64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct PlanView {
    pub application_id: String,
    #[schemars(range(min = 1))]
    pub proposed_generation: u64,
    pub plan: Plan,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct ApplicationStatusView {
    pub application_id: String,
    pub state: String,
    #[schemars(range(min = 1))]
    pub observed_generation: Option<u64>,
    pub message: Option<String>,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct OperationStepView {
    pub id: String,
    pub position: u32,
    pub kind: String,
    pub state: String,
    pub attempt: u32,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct OperationView {
    pub id: String,
    pub application_id: String,
    #[schemars(range(min = 1))]
    pub generation: u64,
    pub kind: String,
    pub state: String,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub steps: Vec<OperationStepView>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct SseEvent {
    pub id: Option<String>,
    pub event: Option<String>,
    pub data: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ListApplicationsOptions {
    pub cursor: Option<String>,
    pub limit: Option<u16>,
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
        let response = self
            .raw_bytes(method, path, body.as_bytes().to_vec(), headers)
            .await?;
        decode_envelope(response).await
    }

    async fn send<T: DeserializeOwned, B: Serialize>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
        headers: &[(&str, &str)],
    ) -> Result<T, ClientError> {
        decode_envelope(self.raw_request(method, path, body, headers).await?).await
    }

    pub async fn system_status(&self) -> Result<SystemStatus, ClientError> {
        self.send::<_, ()>(Method::GET, "/api/v1/system/status", None, &[])
            .await
    }
    pub async fn capabilities(&self) -> Result<SystemCapabilities, ClientError> {
        self.send::<_, ()>(Method::GET, "/api/v1/system/capabilities", None, &[])
            .await
    }
    pub async fn applications(&self) -> Result<Page<ApplicationView>, ClientError> {
        self.applications_with(&ListApplicationsOptions::default())
            .await
    }
    pub async fn applications_with(
        &self,
        options: &ListApplicationsOptions,
    ) -> Result<Page<ApplicationView>, ClientError> {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        if let Some(cursor) = &options.cursor {
            query.append_pair("cursor", cursor);
        }
        if let Some(limit) = options.limit {
            query.append_pair("limit", &limit.to_string());
        }
        let query = query.finish();
        let path = if query.is_empty() {
            "/api/v1/applications".to_owned()
        } else {
            format!("/api/v1/applications?{query}")
        };
        self.send::<_, ()>(Method::GET, &path, None, &[]).await
    }
    pub async fn application(&self, id: &str) -> Result<ApplicationView, ClientError> {
        self.send::<_, ()>(
            Method::GET,
            &format!("/api/v1/applications/{}", path_segment(id)),
            None,
            &[],
        )
        .await
    }
    pub async fn create_application(
        &self,
        request: &CreateApplicationRequest,
        idempotency_key: &str,
    ) -> Result<AcceptedOperation, ClientError> {
        self.send(
            Method::POST,
            "/api/v1/applications",
            Some(request),
            &[("idempotency-key", idempotency_key)],
        )
        .await
    }
    pub async fn replace_application(
        &self,
        id: &str,
        request: &ReplaceApplicationRequest,
    ) -> Result<AcceptedOperation, ClientError> {
        self.send(
            Method::PUT,
            &format!("/api/v1/applications/{}", path_segment(id)),
            Some(request),
            &[],
        )
        .await
    }
    pub async fn delete_application(
        &self,
        id: &str,
        request: &DeleteApplicationRequest,
    ) -> Result<AcceptedOperation, ClientError> {
        self.send(
            Method::DELETE,
            &format!("/api/v1/applications/{}", path_segment(id)),
            Some(request),
            &[],
        )
        .await
    }
    pub async fn plan_create(
        &self,
        request: &PlanApplicationRequest,
    ) -> Result<PlanView, ClientError> {
        self.send(
            Method::POST,
            "/api/v1/applications/plan",
            Some(request),
            &[],
        )
        .await
    }
    pub async fn plan_replace(
        &self,
        id: &str,
        request: &ReplacePlanRequest,
    ) -> Result<PlanView, ClientError> {
        self.send(
            Method::POST,
            &format!("/api/v1/applications/{}/plan", path_segment(id)),
            Some(request),
            &[],
        )
        .await
    }
    pub async fn reconcile(
        &self,
        id: &str,
        expected_generation: u64,
    ) -> Result<AcceptedOperation, ClientError> {
        self.send(
            Method::POST,
            &format!("/api/v1/applications/{}/reconcile", path_segment(id)),
            Some(&ExpectedGeneration {
                expected_generation,
            }),
            &[],
        )
        .await
    }
    pub async fn application_status(&self, id: &str) -> Result<ApplicationStatusView, ClientError> {
        self.send::<_, ()>(
            Method::GET,
            &format!("/api/v1/applications/{}/status", path_segment(id)),
            None,
            &[],
        )
        .await
    }
    pub async fn operation(&self, id: &str) -> Result<OperationView, ClientError> {
        self.send::<_, ()>(
            Method::GET,
            &format!("/api/v1/operations/{}", path_segment(id)),
            None,
            &[],
        )
        .await
    }

    pub async fn openapi(&self) -> Result<serde_json::Value, ClientError> {
        let response = self
            .raw_request::<()>(Method::GET, "/api/v1/openapi.json", None, &[])
            .await?;
        if !response.status().is_success() {
            return Err(decode_api_error(response).await);
        }
        serde_json::from_slice(
            &response
                .into_body()
                .collect()
                .await
                .map_err(|_| ClientError::Transport)?
                .to_bytes(),
        )
        .map_err(|_| ClientError::Decode)
    }

    pub async fn create_application_toml(
        &self,
        manifest: &str,
        idempotency_key: &str,
    ) -> Result<AcceptedOperation, ClientError> {
        self.send_text(
            Method::POST,
            "/api/v1/applications",
            manifest,
            &[
                ("content-type", "application/toml"),
                ("idempotency-key", idempotency_key),
            ],
        )
        .await
    }

    pub async fn replace_application_toml(
        &self,
        id: &str,
        manifest: &str,
        expected_generation: u64,
    ) -> Result<AcceptedOperation, ClientError> {
        let generation = expected_generation.to_string();
        self.send_text(
            Method::PUT,
            &format!("/api/v1/applications/{}", path_segment(id)),
            manifest,
            &[
                ("content-type", "application/toml"),
                ("x-expected-generation", &generation),
            ],
        )
        .await
    }

    pub async fn plan_create_toml(&self, manifest: &str) -> Result<PlanView, ClientError> {
        self.send_text(
            Method::POST,
            "/api/v1/applications/plan",
            manifest,
            &[("content-type", "application/toml")],
        )
        .await
    }

    pub async fn plan_replace_toml(
        &self,
        id: &str,
        manifest: &str,
        expected_generation: u64,
    ) -> Result<PlanView, ClientError> {
        let generation = expected_generation.to_string();
        self.send_text(
            Method::POST,
            &format!("/api/v1/applications/{}/plan", path_segment(id)),
            manifest,
            &[
                ("content-type", "application/toml"),
                ("x-expected-generation", &generation),
            ],
        )
        .await
    }

    /// Watches operation progress. Dropping the receiver cancels socket reading and closes the connection.
    #[must_use]
    pub fn watch_operation(
        &self,
        id: &str,
        last_event_id: Option<&str>,
    ) -> tokio::sync::mpsc::Receiver<Result<SseEvent, ClientError>> {
        self.watch_events(
            format!("/api/v1/operations/{}/events", path_segment(id)),
            last_event_id,
        )
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
                    let _ = tx.send(Err(decode_api_error(response).await)).await;
                    return;
                }
                Err(error) => {
                    let _ = tx.send(Err(error)).await;
                    return;
                }
            };
            let mut body = response.into_body();
            let mut buffer = String::new();
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
                    buffer.push_str(&String::from_utf8_lossy(&data));
                    buffer = buffer.replace("\r\n", "\n");
                    while let Some(end) = buffer.find("\n\n") {
                        let block = buffer[..end].to_owned();
                        buffer.drain(..end + 2);
                        if let Some(event) = parse_sse(&block)
                            && tx.send(Ok(event)).await.is_err()
                        {
                            return;
                        }
                    }
                }
            }
        });
        rx
    }

    #[must_use]
    pub fn watch_application(
        &self,
        id: &str,
        last_event_id: Option<&str>,
    ) -> tokio::sync::mpsc::Receiver<Result<SseEvent, ClientError>> {
        self.watch_events(
            format!("/api/v1/applications/{}/events", path_segment(id)),
            last_event_id,
        )
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

#[must_use]
pub const fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
