//! Public client configuration and adapters around the generated Progenitor client.

use bytes::Bytes;
use futures_util::StreamExt;
use progenitor_client::{ClientHooks, ClientInfo, Error, OperationInfo, ResponseValue};
use serde::de::DeserializeOwned;
use std::time::Duration;

use crate::{ClientError, Envelope, ErrorBody, TransportFailure, generated};

/// Upper bound on buffered response bodies for every operation.
const MAX_RESPONSE_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Default whole-request deadline, and the native connection deadline.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Transport message used for timeouts; `ClientError::from_generated` matches on
/// it to restore `TransportFailure::Timeout` after `decode_response` flattens
/// body-read errors into `Error::Custom`.
const TIMEOUT_MESSAGE: &str = "request timed out";

/// Per-client request settings stored inside the generated client and applied to
/// every outgoing request by `ClientState::prepare`.
#[derive(Clone, Debug)]
pub(crate) struct ClientState {
    /// Whole-request deadline, including reading the response body.
    timeout: Duration,
    /// Sent as `idempotency-key` on mutating requests so retries replay one command.
    request_id: Option<String>,
    /// Pre-built, sensitive `Authorization` header value.
    bearer: Option<reqwest::header::HeaderValue>,
    /// Allows credentials over plain HTTP to non-loopback hosts.
    allow_insecure_http: bool,
}

impl Default for ClientState {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            request_id: None,
            bearer: None,
            allow_insecure_http: false,
        }
    }
}

/// Configured asynchronous API client.
#[derive(Clone, Debug)]
pub struct Client {
    pub(crate) generated: generated::Client,
}

impl Client {
    /// Window event emitted when a browser API request needs a new session.
    #[cfg(target_arch = "wasm32")]
    pub const AUTHENTICATION_REQUIRED_EVENT: &'static str = "piqueld-authentication-required";

    /// Wraps `client` in the generated client with default settings: a 30
    /// second timeout, no credentials, and no request ID.
    fn new(base_url: &str, client: reqwest::Client) -> Self {
        Self {
            generated: generated::Client::new_with_client(base_url, client, ClientState::default()),
        }
    }

    /// Creates a client for an HTTP or HTTPS endpoint.
    ///
    /// Accepts IP addresses and DNS names. Prefer HTTPS for remote access;
    /// Remote HTTP authentication requires an explicit [`Self::with_insecure_http`]
    /// opt-in, even when an encrypted transport such as Tailscale is used.
    ///
    /// # Errors
    /// Returns [`ClientError::Endpoint`] when `base_url` is not an HTTP origin.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn tcp(base_url: &str) -> Result<Self, ClientError> {
        let url = url::Url::parse(base_url)
            .map_err(|_| ClientError::invalid_request("base URL is not a valid URL"))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || base_url.contains('@')
            || url.host().is_none()
        {
            return Err(ClientError::invalid_request(
                "base URL must be an HTTP or HTTPS origin",
            ));
        }
        Self::native(base_url.trim_end_matches('/'), |builder| {
            if url.host_str() != Some("localhost") {
                return builder;
            }
            let port = url.port_or_known_default().unwrap_or(80);
            builder.resolve_to_addrs(
                "localhost",
                &[
                    std::net::SocketAddr::new(std::net::Ipv6Addr::LOCALHOST.into(), port),
                    std::net::SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), port),
                ],
            )
        })
    }

    /// Creates a client for a Unix-domain socket.
    ///
    /// Account credentials are required, just as for the TCP transport.
    ///
    /// # Panics
    /// Panics only if reqwest rejects its fixed, library-owned configuration.
    #[cfg(all(not(target_arch = "wasm32"), unix))]
    pub fn unix(path: impl AsRef<std::path::Path>) -> Self {
        Self::native("http://localhost", |builder| {
            builder.unix_socket(path.as_ref().to_path_buf())
        })
        .expect("the fixed Unix-socket client configuration is valid")
    }

    /// Builds a native reqwest client, which never follows redirects or uses
    /// proxies, after `configure` adds its transport.
    #[cfg(not(target_arch = "wasm32"))]
    fn native(
        base_url: &str,
        configure: impl FnOnce(reqwest::ClientBuilder) -> reqwest::ClientBuilder,
    ) -> Result<Self, ClientError> {
        let builder = reqwest::ClientBuilder::new()
            .connect_timeout(DEFAULT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy();
        let client = configure(builder)
            .build()
            .map_err(|_| ClientError::invalid_request("HTTP client configuration is invalid"))?;
        Ok(Self::new(base_url, client))
    }

    /// Creates a client that fetches the daemon API from the current browser origin.
    #[cfg(target_arch = "wasm32")]
    #[must_use]
    pub fn browser() -> Self {
        let base_url = web_sys::window()
            .and_then(|window| window.location().origin().ok())
            .unwrap_or_default();
        Self::new(&base_url, reqwest::Client::new())
    }

    /// Sets a bearer credential for subsequent requests. Debug output redacts it.
    ///
    /// # Errors
    /// Rejects secrets that cannot be represented as an HTTP header.
    pub fn with_bearer(mut self, token: &str) -> Result<Self, ClientError> {
        let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| ClientError::invalid_request("invalid bearer credential"))?;
        value.set_sensitive(true);
        self.generated.inner.bearer = Some(value);
        Ok(self)
    }

    /// Allows authentication over remote HTTP, including device login secrets.
    /// Only opt in when the connection is protected separately, such as by Tailscale.
    #[must_use]
    pub fn with_insecure_http(mut self) -> Self {
        self.generated.inner.allow_insecure_http = true;
        self
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

    /// Posts a raw `body` of `content_type` to `path` and decodes the data of
    /// its JSON envelope.
    ///
    /// Used for endpoints whose media types Progenitor cannot generate, or
    /// whose bodies need headers it cannot set, such as a streamed body's
    /// `Content-Length`. It reproduces the generated pipeline by hand:
    /// [`Self::execute`] for request preparation and response observation,
    /// the response size cap, and `ErrorBody` decoding for non-success statuses.
    pub(crate) async fn send<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
        headers: &[(&str, String)],
        (content_type, body): (&str, reqwest::Body),
    ) -> Result<T, ClientError> {
        let url = format!("{}{path}", self.generated.baseurl);
        let mut builder = self
            .generated
            .client
            .post(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header("api-version", "v1")
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .query(query)
            .body(body);
        for (name, value) in headers {
            builder = builder.header(*name, value);
        }
        let response = self.execute(builder).await?;
        let status = response.status();
        let payload = collect_response(response).await?;
        if !status.is_success() {
            return Err(ClientError::api(status, &payload));
        }
        serde_json::from_slice::<Envelope<T>>(&payload)
            .map(|envelope| envelope.data)
            .map_err(|source| ClientError::Decode { source })
    }

    /// Sends one handwritten request with the same preparation and response
    /// observation as generated operations.
    pub(crate) async fn execute(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, ClientError> {
        let mut request = builder
            .build()
            .map_err(|_| ClientError::invalid_request("request could not be constructed"))?;
        self.generated
            .inner
            .prepare(&mut request)
            .map_err(ClientError::invalid_request)?;
        let response = self
            .generated
            .client
            .execute(request)
            .await
            .map_err(ClientError::transport)?;
        Self::observe_response(&response);
        Ok(response)
    }

    /// Inspects every response before decoding. In the browser, an unexpected 401
    /// outside the login/registration ceremonies dispatches
    /// `AUTHENTICATION_REQUIRED_EVENT` so the dashboard can prompt for a new session.
    /// No-op on native targets.
    fn observe_response(response: &reqwest::Response) {
        #[cfg(target_arch = "wasm32")]
        if response.status() == reqwest::StatusCode::UNAUTHORIZED
            && !response.url().path().contains("/auth/login/")
            && !response.url().path().contains("/auth/register/")
            && let Some(window) = web_sys::window()
            && let Ok(event) = web_sys::Event::new(Self::AUTHENTICATION_REQUIRED_EVENT)
        {
            let _ = window.dispatch_event(&event);
        }
        #[cfg(not(target_arch = "wasm32"))]
        let _ = response;
    }
}

impl ClientHooks<ClientState> for generated::Client {
    // Request preparation is synchronous, so the hook returns a ready future.
    fn pre<E>(
        &self,
        request: &mut reqwest::Request,
        _info: &OperationInfo,
    ) -> impl Future<Output = Result<(), Error<E>>> {
        std::future::ready(self.inner().prepare(request).map_err(Error::InvalidRequest))
    }

    // Header observation is synchronous as well.
    fn post<E>(
        &self,
        result: &reqwest::Result<reqwest::Response>,
        _info: &OperationInfo,
    ) -> impl Future<Output = Result<(), Error<E>>> {
        if let Ok(response) = result {
            Client::observe_response(response);
        }
        std::future::ready(Ok(()))
    }
}

impl ClientState {
    /// Applies these settings to an outgoing request: enforces the auth transport
    /// policy, sets the timeout, attaches the bearer token, and adds the request ID
    /// as `idempotency-key` on non-GET/HEAD requests that do not already carry one.
    /// Shared by the generated `pre` hook and `Client::execute`.
    fn prepare(&self, request: &mut reqwest::Request) -> Result<(), String> {
        self.validate_auth_transport(request.url())?;
        // reqwest passes this duration to the signed 32-bit browser timer.
        #[cfg(target_arch = "wasm32")]
        i32::try_from(self.timeout.as_millis())
            .map_err(|_| "browser request timeout exceeds i32::MAX milliseconds".to_owned())?;
        *request.timeout_mut() = Some(self.timeout);
        if let Some(bearer) = &self.bearer {
            request
                .headers_mut()
                .insert(reqwest::header::AUTHORIZATION, bearer.clone());
        }
        if request.method() != reqwest::Method::GET
            && request.method() != reqwest::Method::HEAD
            && !request.headers().contains_key("idempotency-key")
            && let Some(request_id) = &self.request_id
        {
            let value = reqwest::header::HeaderValue::from_str(request_id)
                .map_err(|_| "request header value is invalid".to_owned())?;
            request.headers_mut().insert("idempotency-key", value);
        }
        Ok(())
    }

    /// Rejects credentials sent over plain HTTP to a non-loopback host unless the
    /// caller opted in with `Client::with_insecure_http`. A request counts as
    /// authenticated when it carries a bearer token or targets `/api/v1/auth/`.
    ///
    /// ```text
    /// http://localhost:7845 + bearer      -> allowed (loopback)
    /// https://daemon.example + bearer     -> allowed (TLS)
    /// http://daemon.example + bearer      -> rejected without the opt-in
    /// http://daemon.example/api/v1/system -> allowed (no credential)
    /// ```
    fn validate_auth_transport(&self, url: &url::Url) -> Result<(), String> {
        let local = match url.host() {
            Some(url::Host::Domain("localhost")) => true,
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
        if url.scheme() == "http"
            && !local
            && !self.allow_insecure_http
            && (self.bearer.is_some() || url.path().starts_with("/api/v1/auth/"))
        {
            return Err(
                "remote HTTP authentication requires HTTPS or an explicit insecure HTTP opt-in"
                    .into(),
            );
        }
        Ok(())
    }
}

/// Awaits a generated operation and converts its result into the public
/// `Result<T, ClientError>`. Every wrapper around a generated operation ends
/// in [`Self::payload`] or, for enveloped JSON, [`EnvelopedCall::data`]:
///
/// ```text
/// self.generated.get_operation(id).data().await
/// ```
pub(crate) trait GeneratedCall<T, E: ApiErrorPayload>:
    Future<Output = Result<ResponseValue<T>, Error<E>>> + Sized
{
    /// Returns the decoded response body.
    async fn payload(self) -> Result<T, ClientError> {
        match self.await {
            Ok(response) => Ok(response.into_inner()),
            Err(error) => Err(ClientError::from_generated(error).await),
        }
    }
}

impl<F, T, E> GeneratedCall<T, E> for F
where
    F: Future<Output = Result<ResponseValue<T>, Error<E>>>,
    E: ApiErrorPayload,
{
}

/// A generated operation whose JSON response wraps its data in an [`Envelope`].
pub(crate) trait EnvelopedCall<T> {
    /// Returns the envelope's data.
    async fn data(self) -> Result<T, ClientError>;
}

impl<F, T, E> EnvelopedCall<T> for F
where
    F: Future<Output = Result<ResponseValue<Envelope<T>>, Error<E>>>,
    E: ApiErrorPayload,
{
    async fn data(self) -> Result<T, ClientError> {
        self.payload().await.map(|envelope| envelope.data)
    }
}

/// Decodes a JSON response for generated operations, which call this instead of
/// Progenitor's default decoder so every body is subject to `MAX_RESPONSE_BODY_BYTES`.
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

/// Buffers a full response body, enforcing the size cap.
async fn collect_response(response: reqwest::Response) -> Result<Vec<u8>, ClientError> {
    collect_stream(response.bytes_stream()).await
}

/// Buffers a generated streaming (non-JSON) response body, enforcing the size cap.
pub(crate) async fn collect_byte_stream(
    response: progenitor_client::ByteStream,
) -> Result<Vec<u8>, ClientError> {
    collect_stream(response.into_inner()).await
}

/// Concatenates body chunks, failing with a transport error as soon as the total
/// would exceed `MAX_RESPONSE_BODY_BYTES` rather than buffering unbounded output.
async fn collect_stream<S>(mut stream: S) -> Result<Vec<u8>, ClientError>
where
    S: futures_util::Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    let mut buffer = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(ClientError::transport)?;
        if buffer.len().saturating_add(chunk.len()) > MAX_RESPONSE_BODY_BYTES {
            return Err(ClientError::exchange(format!(
                "response body exceeded the {MAX_RESPONSE_BODY_BYTES}-byte limit"
            )));
        }
        buffer.extend_from_slice(&chunk);
    }
    Ok(buffer)
}

/// Rejects a page `limit` outside `1..=max` before sending; `list` names the
/// listing in the error, such as `event`.
pub(crate) fn check_page_limit(list: &str, limit: u16, max: u16) -> Result<(), ClientError> {
    if (1..=max).contains(&limit) {
        Ok(())
    } else {
        Err(ClientError::invalid_request(format!(
            "{list} limit must be between 1 and {max}"
        )))
    }
}

/// Documented error payload types of generated operations. Converts the payload into
/// an `ErrorBody` when it has one; `None` makes `ClientError::from_generated` report
/// an undocumented status instead.
pub(crate) trait ApiErrorPayload {
    /// Returns the structured API error carried by this payload, if any.
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

// Readiness documents its 503 as a normal readiness payload; `Client::system_readiness`
// handles it before this conversion is reached.
impl ApiErrorPayload for Envelope<piqueld_core::api::ReadinessStatus> {
    fn into_error_body(self) -> Option<ErrorBody> {
        None
    }
}

impl ClientError {
    /// Whether the daemon answered that the requested resource does not exist.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Api { status, .. } if *status == http::StatusCode::NOT_FOUND)
    }

    /// Builds an `Endpoint` error for requests rejected before they are sent.
    pub(crate) fn invalid_request(message: impl std::fmt::Display) -> Self {
        Self::Endpoint {
            message: message.to_string(),
        }
    }

    /// Builds a `Transport` error for a failed exchange on an open connection.
    pub(crate) fn exchange(message: impl Into<String>) -> Self {
        Self::Transport {
            message: message.into(),
            kind: TransportFailure::Exchange,
        }
    }

    /// Transport error for a status the `OpenAPI` document does not describe.
    fn unexpected_status(status: reqwest::StatusCode) -> Self {
        Self::exchange(format!("server returned undocumented status {status}"))
    }

    /// Classifies a reqwest failure as a timeout, a connection failure (the first
    /// `io::Error` kind in the source chain, or `ConnectionRefused` for connect
    /// errors without one), or a generic exchange failure. Other messages join
    /// the error and its whole source chain so the root cause is visible:
    ///
    /// ```text
    /// <reqwest error>: <hyper cause>: Connection refused (os error 111)
    /// ```
    // `map_err` passes ownership; retaining reqwest's error beyond this conversion is unnecessary.
    #[allow(clippy::needless_pass_by_value)]
    fn transport(error: reqwest::Error) -> Self {
        use std::error::Error as _;

        if error.is_timeout() {
            return Self::Transport {
                message: TIMEOUT_MESSAGE.to_owned(),
                kind: TransportFailure::Timeout,
            };
        }
        let causes = || std::iter::successors(error.source(), |&cause| cause.source());
        #[cfg(not(target_arch = "wasm32"))]
        let connection = causes()
            .find_map(|cause| cause.downcast_ref::<std::io::Error>())
            .map(std::io::Error::kind)
            .or_else(|| {
                error
                    .is_connect()
                    .then_some(std::io::ErrorKind::ConnectionRefused)
            });
        #[cfg(target_arch = "wasm32")]
        let connection = None;
        let mut message = error.to_string();
        for cause in causes() {
            message.push_str(": ");
            message.push_str(&cause.to_string());
        }
        Self::Transport {
            message,
            kind: connection.map_or(TransportFailure::Exchange, TransportFailure::Connect),
        }
    }

    /// Decodes a non-success body as an `ErrorBody`, substituting a synthetic
    /// `invalid_error_response` body when the server's error is not valid JSON.
    fn api(status: reqwest::StatusCode, payload: &[u8]) -> Self {
        Self::Api {
            status,
            error: serde_json::from_slice(payload).unwrap_or_else(|error| ErrorBody {
                code: "invalid_error_response".into(),
                message: format!("server returned an unreadable error: {error}"),
                details: serde_json::Value::Null,
                request_id: String::new(),
            }),
        }
    }

    /// Converts an unsuccessful handwritten response into its API error.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) async fn from_response(response: reqwest::Response) -> Self {
        let status = response.status();
        match collect_response(response).await {
            Ok(payload) => Self::api(status, &payload),
            Err(error) => error,
        }
    }

    /// Maps Progenitor's error variants onto `ClientError`. Documented error
    /// responses become `ClientError::Api`; undocumented ones have their body read
    /// and decoded as an `ErrorBody` when the status is a failure.
    pub(crate) async fn from_generated<E: ApiErrorPayload>(error: Error<E>) -> Self {
        match error {
            Error::InvalidRequest(message) => Self::invalid_request(message),
            Error::CommunicationError(error)
            | Error::InvalidUpgrade(error)
            | Error::ResponseBodyError(error) => Self::transport(error),
            Error::ErrorResponse(response) => {
                let status = response.status();
                response.into_inner().into_error_body().map_or_else(
                    || Self::unexpected_status(status),
                    |error| Self::Api { status, error },
                )
            }
            Error::InvalidResponsePayload(_, source) => Self::Decode { source },
            Error::UnexpectedResponse(response) => {
                let status = response.status();
                match collect_response(response).await {
                    Ok(payload) if !status.is_success() => Self::api(status, &payload),
                    Ok(_) => Self::unexpected_status(status),
                    Err(error) => error,
                }
            }
            Error::Custom(message) if message == TIMEOUT_MESSAGE => Self::Transport {
                message,
                kind: TransportFailure::Timeout,
            },
            Error::Custom(message) => Self::exchange(message),
        }
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::{Client, ClientError, MAX_RESPONSE_BODY_BYTES, collect_stream};
    use bytes::Bytes;
    use futures_util::stream;
    use std::time::Duration;
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
            .with_timeout(Duration::from_millis(u64::try_from(i32::MAX).unwrap() + 1))
            .system_status()
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ClientError::Endpoint { message } if message.contains("timeout exceeds")
        ));
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::Client;

    #[test]
    fn authentication_transport_policy_requires_explicit_remote_http_opt_in() {
        for endpoint in [
            "https://daemon.example",
            "http://localhost:7845",
            "http://127.0.0.1:7845",
            "http://[::1]:7845",
            "http://100.64.0.1:7845",
            "http://daemon.example",
        ] {
            for opt_in in [false, true] {
                let mut client = Client::tcp(endpoint)
                    .unwrap()
                    .with_bearer("secret")
                    .unwrap();
                if opt_in {
                    client = client.with_insecure_http();
                }
                let mut request =
                    reqwest::Request::new(reqwest::Method::GET, url::Url::parse(endpoint).unwrap());
                let allowed = opt_in
                    || endpoint.starts_with("https:")
                    || endpoint.contains("localhost")
                    || endpoint.contains("127.0.0.1")
                    || endpoint.contains("[::1]");
                assert_eq!(
                    client.generated.inner.prepare(&mut request).is_ok(),
                    allowed
                );
                assert_eq!(request.headers().contains_key("authorization"), allowed);
            }
        }
        let client = Client::unix("/tmp/piqueld-test.sock")
            .with_bearer("secret")
            .unwrap();
        let mut request = reqwest::Request::new(
            reqwest::Method::GET,
            url::Url::parse("http://localhost/api/v1/auth/me").unwrap(),
        );
        client.generated.inner.prepare(&mut request).unwrap();
        assert!(request.headers().contains_key("authorization"));
    }
}
