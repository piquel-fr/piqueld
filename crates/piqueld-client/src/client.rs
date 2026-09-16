//! Public client configuration and adapters around the generated Progenitor client.

use bytes::Bytes;
use futures_util::StreamExt;
use progenitor_client::{ClientHooks, ClientInfo, Error, OperationInfo, ResponseValue};
use serde::de::DeserializeOwned;
use std::time::Duration;

use crate::{ClientError, ErrorBody, TransportFailure, generated};

/// Upper bound on buffered response bodies for every operation.
const MAX_RESPONSE_BODY_BYTES: usize = 16 * 1024 * 1024;
const TIMEOUT_MESSAGE: &str = "request timed out";

#[derive(Clone, Debug)]
pub(crate) struct ClientState {
    timeout: Duration,
    request_id: Option<String>,
}

#[derive(Clone, Debug)]
/// Configured asynchronous API client.
pub struct Client {
    pub(crate) generated: generated::Client,
}

impl Client {
    /// Creates a client for an HTTP endpoint.
    ///
    /// Accepts IP addresses and DNS names. HTTP has no application-layer
    /// encryption: use a trusted network such as Tailscale for remote access.
    ///
    /// # Errors
    /// Returns [`ClientError::Endpoint`] when `base_url` is not an HTTP origin.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn tcp(base_url: &str) -> Result<Self, ClientError> {
        let url = url::Url::parse(base_url)
            .map_err(|_| invalid_request("base URL is not a valid URL"))?;
        if url.scheme() != "http"
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || base_url.contains('@')
            || url.host().is_none()
        {
            return Err(invalid_request("base URL must be a plain HTTP origin"));
        }

        let mut builder = reqwest::ClientBuilder::new()
            .connect_timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy();
        if url.host_str() == Some("localhost") {
            let port = url.port().unwrap_or(80);
            builder = builder.resolve_to_addrs(
                "localhost",
                &[
                    std::net::SocketAddr::new(std::net::Ipv6Addr::LOCALHOST.into(), port),
                    std::net::SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), port),
                ],
            );
        }
        Self::with_client(base_url.trim_end_matches('/'), builder)
    }

    /// Creates a client for a Unix-domain socket.
    ///
    /// # Trust model
    /// Any process able to reach `path` can drive the daemon. Only pass paths
    /// provisioned by the piqueld daemon itself.
    ///
    /// # Panics
    /// Panics only if reqwest rejects its fixed, library-owned configuration.
    #[cfg(all(not(target_arch = "wasm32"), unix))]
    pub fn unix(path: impl AsRef<std::path::Path>) -> Self {
        Self::with_client(
            "http://localhost",
            reqwest::ClientBuilder::new()
                .connect_timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .unix_socket(path.as_ref().to_path_buf()),
        )
        .expect("the fixed Unix-socket client configuration is valid")
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn with_client(base_url: &str, builder: reqwest::ClientBuilder) -> Result<Self, ClientError> {
        let client = builder
            .build()
            .map_err(|_| invalid_request("HTTP client configuration is invalid"))?;
        Ok(Self {
            generated: generated::Client::new_with_client(
                base_url,
                client,
                ClientState {
                    timeout: Duration::from_secs(30),
                    request_id: None,
                },
            ),
        })
    }

    /// Creates a client that fetches the daemon API from the current browser origin.
    #[cfg(target_arch = "wasm32")]
    pub fn browser() -> Self {
        let base_url = web_sys::window()
            .and_then(|window| window.location().origin().ok())
            .unwrap_or_default();
        Self {
            generated: generated::Client::new_with_client(
                &base_url,
                reqwest::Client::new(),
                ClientState {
                    timeout: Duration::from_secs(30),
                    request_id: None,
                },
            ),
        }
    }

    /// Overrides the per-request timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.generated.inner.timeout = timeout;
        self
    }

    /// Sets one command's replay identity. Reuse this client for transport retries only;
    /// use a new identity for a separately intended mutation.
    #[must_use]
    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.generated.inner.request_id = Some(request_id.into());
        self
    }

    pub(crate) async fn send_toml<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
        headers: &[(&str, String)],
        body: &str,
    ) -> Result<T, ClientError> {
        let url = format!("{}{path}", self.generated.baseurl);
        let mut builder = self
            .generated
            .client
            .post(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header("api-version", "v1")
            .header(reqwest::header::CONTENT_TYPE, "application/toml")
            .query(query)
            .body(body.to_owned());
        for (name, value) in headers {
            builder = builder.header(*name, value);
        }
        let mut request = builder
            .build()
            .map_err(|_| invalid_request("request could not be constructed"))?;
        prepare_request(&self.generated.inner, &mut request)
            .map_err(|_| invalid_request("request header or timeout is invalid"))?;
        let response = self
            .generated
            .client
            .execute(request)
            .await
            .map_err(transport_error)?;
        let status = response.status();
        let payload = collect_response(response).await?;
        if !status.is_success() {
            return Err(api_error(status, &payload));
        }
        serde_json::from_slice(&payload).map_err(|source| ClientError::Decode { source })
    }
}

impl ClientHooks<ClientState> for generated::Client {
    // The external async trait fixes this signature even though preparation is synchronous.
    #[allow(clippy::unused_async_trait_impl)]
    async fn pre<E>(
        &self,
        request: &mut reqwest::Request,
        _info: &OperationInfo,
    ) -> Result<(), Error<E>> {
        prepare_request(self.inner(), request).map_err(Error::InvalidRequest)
    }
}

fn prepare_request(state: &ClientState, request: &mut reqwest::Request) -> Result<(), String> {
    #[cfg(target_arch = "wasm32")]
    u32::try_from(state.timeout.as_millis())
        .map_err(|_| "browser request timeout exceeds u32::MAX milliseconds".to_owned())?;
    *request.timeout_mut() = Some(state.timeout);
    if request.method() != reqwest::Method::GET
        && request.method() != reqwest::Method::HEAD
        && !request.headers().contains_key("idempotency-key")
        && let Some(request_id) = &state.request_id
    {
        let value = reqwest::header::HeaderValue::from_str(request_id)
            .map_err(|_| "request header value is invalid".to_owned())?;
        request.headers_mut().insert("idempotency-key", value);
    }
    Ok(())
}

// Progenitor fixes this public error type; boxing it would not match generated calls.
#[allow(clippy::result_large_err)]
pub(crate) async fn decode_response<T, E>(
    response: reqwest::Response,
) -> Result<ResponseValue<T>, Error<E>>
where
    T: DeserializeOwned,
{
    let status = response.status();
    let headers = response.headers().clone();
    let full = collect_response(response)
        .await
        .map_err(|error| match error {
            ClientError::Transport { message, .. } => Error::Custom(message),
            _ => Error::Custom(error.to_string()),
        })?;
    let full = Bytes::from(full);
    let value = serde_json::from_slice(&full)
        .map_err(|error| Error::InvalidResponsePayload(full, error))?;
    Ok(ResponseValue::new(value, status, headers))
}

async fn collect_response(response: reqwest::Response) -> Result<Vec<u8>, ClientError> {
    collect_stream(response.bytes_stream()).await
}

pub(crate) async fn collect_byte_stream(
    response: progenitor_client::ByteStream,
) -> Result<Vec<u8>, ClientError> {
    collect_stream(response.into_inner()).await
}

async fn collect_stream<S>(mut stream: S) -> Result<Vec<u8>, ClientError>
where
    S: futures_util::Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    let mut buffer = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(transport_error)?;
        if buffer.len().saturating_add(chunk.len()) > MAX_RESPONSE_BODY_BYTES {
            return Err(ClientError::Transport {
                message: format!("response body exceeded the {MAX_RESPONSE_BODY_BYTES}-byte limit"),
                kind: TransportFailure::Exchange,
            });
        }
        buffer.extend_from_slice(&chunk);
    }
    Ok(buffer)
}

pub(crate) trait ApiErrorPayload {
    fn into_error_body(self) -> Option<ErrorBody>;
}

impl ApiErrorPayload for ErrorBody {
    fn into_error_body(self) -> Option<ErrorBody> {
        Some(self)
    }
}

impl ApiErrorPayload for () {
    fn into_error_body(self) -> Option<ErrorBody> {
        None
    }
}

impl ApiErrorPayload for crate::Envelope<piqueld_core::api::ReadinessStatus> {
    fn into_error_body(self) -> Option<ErrorBody> {
        None
    }
}

pub(crate) async fn generated_result<T, E>(
    result: Result<ResponseValue<T>, Error<E>>,
) -> Result<T, ClientError>
where
    E: ApiErrorPayload,
{
    match result {
        Ok(response) => Ok(response.into_inner()),
        Err(error) => Err(generated_error(error).await),
    }
}

pub(crate) async fn generated_error<E>(error: Error<E>) -> ClientError
where
    E: ApiErrorPayload,
{
    match error {
        Error::InvalidRequest(message) => invalid_request(message),
        Error::CommunicationError(error)
        | Error::InvalidUpgrade(error)
        | Error::ResponseBodyError(error) => transport_error(error),
        Error::ErrorResponse(response) => {
            let status = response.status();
            response.into_inner().into_error_body().map_or_else(
                || unexpected_status(status),
                |error| ClientError::Api { status, error },
            )
        }
        Error::InvalidResponsePayload(_, source) => ClientError::Decode { source },
        Error::UnexpectedResponse(response) => {
            let status = response.status();
            match collect_response(response).await {
                Ok(payload) if !status.is_success() => api_error(status, &payload),
                Ok(_) => unexpected_status(status),
                Err(error) => error,
            }
        }
        Error::Custom(message) => ClientError::Transport {
            kind: if message == TIMEOUT_MESSAGE {
                TransportFailure::Timeout
            } else {
                TransportFailure::Exchange
            },
            message,
        },
    }
}

fn unexpected_status(status: reqwest::StatusCode) -> ClientError {
    ClientError::Transport {
        message: format!("server returned undocumented status {status}"),
        kind: TransportFailure::Exchange,
    }
}

// `map_err` passes ownership; retaining reqwest's error beyond this conversion is unnecessary.
#[allow(clippy::needless_pass_by_value)]
fn transport_error(error: reqwest::Error) -> ClientError {
    #[cfg(not(target_arch = "wasm32"))]
    let connection_kind = connection_error_kind(&error).or_else(|| {
        error
            .is_connect()
            .then_some(std::io::ErrorKind::ConnectionRefused)
    });
    #[cfg(target_arch = "wasm32")]
    let connection_kind = None;
    let kind = if error.is_timeout() {
        TransportFailure::Timeout
    } else if let Some(kind) = connection_kind {
        TransportFailure::Connect(kind)
    } else {
        TransportFailure::Exchange
    };
    ClientError::Transport {
        message: if matches!(kind, TransportFailure::Timeout) {
            TIMEOUT_MESSAGE.to_owned()
        } else {
            transport_error_message(&error)
        },
        kind,
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn connection_error_kind(error: &reqwest::Error) -> Option<std::io::ErrorKind> {
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        if let Some(error) = cause.downcast_ref::<std::io::Error>() {
            return Some(error.kind());
        }
        source = cause.source();
    }
    None
}

fn transport_error_message(error: &reqwest::Error) -> String {
    use std::fmt::Write as _;

    let mut message = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let _ = write!(message, ": {cause}");
        source = cause.source();
    }
    message
}

pub(crate) fn invalid_request(message: impl std::fmt::Display) -> ClientError {
    ClientError::Endpoint {
        message: message.to_string(),
    }
}

fn api_error(status: reqwest::StatusCode, payload: &[u8]) -> ClientError {
    ClientError::Api {
        status,
        error: serde_json::from_slice(payload).unwrap_or_else(|error| ErrorBody {
            code: "invalid_error_response".into(),
            message: format!("server returned an unreadable error: {error}"),
            details: serde_json::Value::Null,
            request_id: String::new(),
        }),
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::{Client, ClientError, MAX_RESPONSE_BODY_BYTES, collect_stream};
    use bytes::Bytes;
    use futures_util::stream;
    use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};

    wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen_test]
    async fn response_collection_rejects_oversized_streams() {
        let chunks = stream::iter([
            Ok::<_, reqwest::Error>(Bytes::from(vec![0; MAX_RESPONSE_BODY_BYTES])),
            Ok(Bytes::from_static(&[1])),
        ]);
        assert!(matches!(
            collect_stream(chunks).await,
            Err(ClientError::Transport { message, .. }) if message.contains("16")
        ));
    }

    #[wasm_bindgen_test]
    async fn oversized_timeout_is_rejected_before_fetch() {
        let error = Client::browser()
            .with_timeout(Duration::from_millis(u64::from(u32::MAX) + 1))
            .system_status()
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ClientError::Endpoint { message } if message.contains("timeout exceeds")
        ));
    }

    use std::time::Duration;
}
