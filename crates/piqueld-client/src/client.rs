//! The typed API client with one request pipeline for every target.
//!
//! Endpoint methods, envelope decoding, and query building are shared. The
//! platform differences are confined to two small modules: [`loopback`]
//! speaks HTTP/1.1 over loopback TCP and Unix-domain sockets natively, and
//! [`web`] performs same-origin fetches in the browser. Both transports enforce
//! one total deadline per exchange and reject response bodies beyond a fixed
//! size; the native transport also constructs an explicit origin-form request.

use http::{HeaderName, HeaderValue, Method, StatusCode};
use serde::{Serialize, de::DeserializeOwned};
use std::time::Duration;

use crate::{ClientError, Envelope, ErrorBody};

/// Transport detail reported when a request exceeds its deadline.
const TIMEOUT_MESSAGE: &str = "request timed out";

/// Upper bound on buffered response bodies for every exchange.
const MAX_RESPONSE_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Builds the timeout [`ClientError`] shared by both transports.
fn timed_out() -> ClientError {
    ClientError::Transport {
        message: TIMEOUT_MESSAGE.to_owned(),
    }
}

/// Builds [`ClientError::Endpoint`] with the reason construction failed.
pub(crate) fn invalid_request(message: impl std::fmt::Display) -> ClientError {
    ClientError::Endpoint {
        message: message.to_string(),
    }
}

/// Validates shared request headers before either transport sees them.
fn validate_headers(headers: &[(&str, &str)]) -> Result<(), ClientError> {
    for (name, value) in headers {
        HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| invalid_request("request header name is invalid"))?;
        if !value.is_ascii() {
            return Err(invalid_request("request header value must be ASCII"));
        }
        HeaderValue::from_str(value)
            .map_err(|_| invalid_request("request header value is invalid"))?;
    }
    Ok(())
}

/// Builds the bounded-response error shared by both transports.
fn response_too_large(limit: usize) -> ClientError {
    ClientError::Transport {
        message: format!("response body exceeded the {limit}-byte limit"),
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod loopback {
    //! Native transport: HTTP/1.1 over loopback TCP and Unix-domain sockets.

    use bytes::Bytes;
    use http::{Method, Request, StatusCode, header};
    use http_body_util::{BodyExt, Full};
    use hyper::Response;
    use hyper::body::Incoming;
    use hyper::client::conn::http1;
    use hyper_util::rt::TokioIo;
    #[cfg(unix)]
    use std::path::PathBuf;
    use std::{
        net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
        time::Duration,
    };
    use tokio::net::TcpStream;
    #[cfg(unix)]
    use tokio::net::UnixStream;
    use url::{Host, Url};

    use super::{
        ClientError, MAX_RESPONSE_BODY_BYTES, invalid_request, response_too_large, timed_out,
        transport,
    };

    #[derive(Clone, Debug)]
    pub(super) enum Endpoint {
        Tcp {
            authority: String,
            addresses: Vec<SocketAddr>,
        },
        #[cfg(unix)]
        Unix(PathBuf),
    }

    /// Parses a loopback HTTP origin into a connectable endpoint.
    ///
    /// Only plain HTTP origins are accepted: the host must be `localhost`,
    /// an IPv4 loopback address, or IPv6 `::1`. The client is meant for
    /// daemons on the operator's own machine; remote management is out of
    /// scope by design.
    ///
    /// # Errors
    /// Returns [`ClientError::Endpoint`] when `base_url` is not a loopback
    /// HTTP origin.
    pub(super) fn tcp_endpoint(base_url: &str) -> Result<Endpoint, ClientError> {
        let url =
            Url::parse(base_url).map_err(|_| invalid_request("base URL is not a valid URL"))?;
        // Trailing-dot hosts ("localhost.") are rejected on purpose: the
        // resolver may answer differently than for the bare name.
        //
        // Userinfo is rejected wholesale via '@': the parser collapses
        // spellings like ":@" into invisible empty credentials, and with
        // path, query, and fragment already excluded, an '@' can only ever
        // belong to userinfo.
        if url.scheme() != "http"
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || base_url.contains('@')
        {
            return Err(invalid_request(
                "base URL must be a plain loopback HTTP origin",
            ));
        }
        let (host, addresses) = match url
            .host()
            .ok_or_else(|| invalid_request("base URL has no host"))?
        {
            // WHATWG parsing canonicalizes numeric spellings such as "127.1"
            // before this match runs, so acceptance always implies a genuine
            // loopback connect target.
            Host::Domain(host) if host.eq_ignore_ascii_case("localhost") => (
                host.to_ascii_lowercase(),
                vec![
                    IpAddr::V6(Ipv6Addr::LOCALHOST),
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                ],
            ),
            Host::Ipv4(host) if host.is_loopback() => (host.to_string(), vec![IpAddr::V4(host)]),
            Host::Ipv6(host) if host.is_loopback() => (host.to_string(), vec![IpAddr::V6(host)]),
            _ => {
                return Err(invalid_request(
                    "base URL host must be localhost or a loopback IP",
                ));
            }
        };
        // http URLs always carry the implicit default port.
        let port = url
            .port_or_known_default()
            .expect("http URLs have a default port");
        let authority = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        let addresses = addresses
            .into_iter()
            .map(|address| SocketAddr::new(address, port))
            .collect();
        Ok(Endpoint::Tcp {
            authority,
            addresses,
        })
    }

    /// Sends one request and returns the status plus the collected body.
    ///
    /// The deadline governs dispatch and body collection alike, so a stalled
    /// response cannot outlive [`Client::with_timeout`]. Bodies larger than
    /// [`MAX_RESPONSE_BODY_BYTES`] are rejected instead of buffered.
    pub(super) async fn exchange(
        endpoint: &Endpoint,
        timeout: Duration,
        method: Method,
        path: &str,
        payload: Vec<u8>,
        headers: &[(&str, &str)],
    ) -> Result<(StatusCode, Vec<u8>), ClientError> {
        let (status, body) = tokio::time::timeout(timeout, async {
            let response = send_request(endpoint, method, path, payload, headers).await?;
            let status = response.status();
            let body = collect_bounded(response.into_body()).await?;
            Ok((status, body))
        })
        .await
        .map_err(|_| timed_out())??;
        Ok((status, body))
    }

    /// Collects the body while enforcing [`MAX_RESPONSE_BODY_BYTES`].
    async fn collect_bounded(mut body: Incoming) -> Result<Vec<u8>, ClientError> {
        let mut buffer = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(transport)?;
            if let Some(data) = frame.data_ref() {
                if buffer.len().saturating_add(data.len()) > MAX_RESPONSE_BODY_BYTES {
                    return Err(response_too_large(MAX_RESPONSE_BODY_BYTES));
                }
                buffer.extend_from_slice(data);
            }
        }
        Ok(buffer)
    }

    async fn send_request(
        endpoint: &Endpoint,
        method: Method,
        path: &str,
        payload: Vec<u8>,
        headers: &[(&str, &str)],
    ) -> Result<Response<Incoming>, ClientError> {
        let authority = match endpoint {
            Endpoint::Tcp { authority, .. } => authority.as_str(),
            #[cfg(unix)]
            Endpoint::Unix(_) => "localhost",
        };
        if !path.starts_with('/') {
            return Err(invalid_request("request path must start with '/'"));
        }
        // Origin-form target plus an explicit Host header: the raw hyper
        // connection API never fills either in, and RFC 9112 requires both.
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, authority)
            .header(header::ACCEPT, "application/json");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = builder
            .body(Full::new(Bytes::from(payload)))
            .map_err(|error| invalid_request(format!("malformed request head: {error}")))?;
        match endpoint {
            Endpoint::Tcp { addresses, .. } => {
                speak(TcpStream::connect(addresses.as_slice()).await, request).await
            }
            #[cfg(unix)]
            Endpoint::Unix(path) => speak(UnixStream::connect(path).await, request).await,
        }
    }

    async fn speak<S>(
        io: Result<S, std::io::Error>,
        request: Request<Full<Bytes>>,
    ) -> Result<Response<Incoming>, ClientError>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sender, connection) = http1::handshake(TokioIo::new(io.map_err(transport)?))
            .await
            .map_err(transport)?;
        // Drive the connection to completion in the background. Dropping the
        // request or response handles makes hyper close the socket, so this
        // task never outlives the exchange by more than a graceful shutdown.
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::debug!(%error, "piqueld-client connection closed");
            }
        });
        sender.send_request(request).await.map_err(transport)
    }
}

#[cfg(target_arch = "wasm32")]
mod web {
    //! Browser transport: same-origin fetches via `gloo-net`.

    use gloo_net::http::{Request, RequestBuilder, Response};
    use http::{Method, StatusCode};
    use js_sys::{Reflect, Uint8Array};
    use std::time::Duration;
    use wasm_bindgen::{JsCast, JsValue};
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{
        ReadableStreamDefaultReader, RequestCache, RequestCredentials, RequestMode, RequestRedirect,
    };

    use super::{
        ClientError, MAX_RESPONSE_BODY_BYTES, invalid_request, response_too_large, timed_out,
        transport,
    };

    /// Sends one same-origin request and returns the status plus body bytes.
    pub(super) async fn exchange(
        timeout: Duration,
        method: Method,
        path: &str,
        payload: Vec<u8>,
        headers: &[(&str, &str)],
    ) -> Result<(StatusCode, Vec<u8>), ClientError> {
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let signal = web_sys::AbortSignal::timeout_with_u32(millis);
        let mut request = match method {
            Method::GET => Request::get(path),
            Method::POST => Request::post(path),
            Method::PUT => Request::put(path),
            Method::DELETE => Request::delete(path),
            _ => RequestBuilder::new(path).method(method),
        }
        .header("accept", "application/json")
        .cache(RequestCache::NoStore)
        .credentials(RequestCredentials::Omit)
        .mode(RequestMode::SameOrigin)
        // Match the native transport, which never follows redirects.
        .redirect(RequestRedirect::Error);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        // The browser enforces the deadline through the abort signal, which
        // rejects the fetch and any pending body read once it fires. This
        // keeps the native per-request timer semantics on every target.
        let request = request.abort_signal(Some(&signal));
        let sent = if payload.is_empty() {
            request.send().await
        } else {
            // JSON and TOML payloads are always UTF-8.
            let body = String::from_utf8(payload)
                .map_err(|_| invalid_request("request payload is not valid UTF-8"))?;
            request.body(body).map_err(transport)?.send().await
        };
        let response = match sent {
            Ok(response) => response,
            Err(error) => {
                return Err(if signal.aborted() {
                    timed_out()
                } else {
                    transport(error)
                });
            }
        };
        let status = StatusCode::from_u16(response.status()).map_err(|_| {
            transport(format!(
                "server returned invalid status {}",
                response.status()
            ))
        })?;
        let body = match collect_bounded(&response, MAX_RESPONSE_BODY_BYTES).await {
            Ok(body) => body,
            Err(error) => {
                return Err(if signal.aborted() { timed_out() } else { error });
            }
        };
        Ok((status, body))
    }

    /// Drains a fetch body as exact bytes without allowing unbounded buffering.
    async fn collect_bounded(response: &Response, limit: usize) -> Result<Vec<u8>, ClientError> {
        let Some(stream) = response.body() else {
            return Ok(Vec::new());
        };
        let reader: ReadableStreamDefaultReader = stream.get_reader().unchecked_into();
        let mut buffer = Vec::new();
        loop {
            let result = JsFuture::from(reader.read()).await.map_err(js_transport)?;
            let done = Reflect::get(&result, &JsValue::from_str("done"))
                .map_err(js_transport)?
                .as_bool()
                .unwrap_or(false);
            if done {
                reader.release_lock();
                return Ok(buffer);
            }
            let value = Reflect::get(&result, &JsValue::from_str("value")).map_err(js_transport)?;
            let chunk = Uint8Array::new(&value);
            let chunk_len = usize::try_from(chunk.length())
                .map_err(|_| transport("response chunk length exceeds this platform"))?;
            if buffer.len().saturating_add(chunk_len) > limit {
                let _ = reader.cancel();
                reader.release_lock();
                return Err(response_too_large(limit));
            }
            let start = buffer.len();
            buffer.resize(start + chunk_len, 0);
            chunk.copy_to(&mut buffer[start..]);
        }
    }

    fn js_transport(error: JsValue) -> ClientError {
        transport(format!("{error:?}"))
    }

    #[cfg(test)]
    mod tests {
        use super::{MAX_RESPONSE_BODY_BYTES, collect_bounded};
        use crate::{Client, ClientError};
        use http::Method;
        use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};

        wasm_bindgen_test_configure!(run_in_browser);

        fn response(bytes: &mut [u8]) -> gloo_net::http::Response {
            web_sys::Response::new_with_opt_u8_array(Some(bytes))
                .expect("test response is valid")
                .into()
        }

        #[wasm_bindgen_test]
        async fn response_collection_preserves_exact_bytes() {
            let mut bytes = [b'{', b'"', 0xff, b'"', b'}'];
            let response = response(&mut bytes);
            assert_eq!(
                collect_bounded(&response, MAX_RESPONSE_BODY_BYTES)
                    .await
                    .expect("small response is collected"),
                bytes
            );
        }

        #[wasm_bindgen_test]
        async fn response_collection_rejects_the_first_oversized_chunk() {
            let mut bytes = [1, 2, 3, 4, 5];
            let response = response(&mut bytes);
            assert!(matches!(
                collect_bounded(&response, 4).await,
                Err(ClientError::Transport { message }) if message.contains("4-byte limit")
            ));
        }

        #[wasm_bindgen_test]
        async fn exchange_fetches_from_the_current_origin() {
            let (status, payload) = Client::browser()
                .exchange(Method::GET, "/", Vec::new(), &[])
                .await
                .expect("same-origin fetch succeeds");

            assert!(status.is_success());
            assert!(!payload.is_empty());
        }

        #[wasm_bindgen_test]
        async fn invalid_headers_return_an_error_before_fetch() {
            let result = Client::browser()
                .exchange(Method::GET, "/", Vec::new(), &[("x-test", "bad\nvalue")])
                .await;
            assert!(matches!(result, Err(ClientError::Endpoint { .. })));
        }
    }
}

#[derive(Clone, Debug)]
/// Configured asynchronous API client.
pub struct Client {
    #[cfg(not(target_arch = "wasm32"))]
    endpoint: loopback::Endpoint,
    timeout: Duration,
}

impl Client {
    /// Creates a client for an HTTP endpoint.
    ///
    /// Only loopback origins are supported: `localhost`, IPv4 addresses in
    /// `127.0.0.0/8`, and `[::1]`. piqueld daemons listen on the operator's
    /// own machine; remote management is out of scope by design.
    ///
    /// # Errors
    /// Returns [`ClientError::Endpoint`] when `base_url` is not a loopback HTTP origin.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn tcp(base_url: &str) -> Result<Self, ClientError> {
        Ok(Self {
            endpoint: loopback::tcp_endpoint(base_url)?,
            timeout: Duration::from_secs(30),
        })
    }

    /// Creates a client for a Unix-domain socket.
    ///
    /// # Trust model
    /// Any process able to reach `path` can drive the daemon, and this client
    /// speaks plain HTTP to whatever socket it is given — including sockets
    /// owned by other subsystems such as the Docker socket. Only pass paths
    /// provisioned by the piqueld daemon itself.
    #[cfg(all(not(target_arch = "wasm32"), unix))]
    #[must_use]
    pub fn unix(path: impl AsRef<std::path::Path>) -> Self {
        Self {
            endpoint: loopback::Endpoint::Unix(path.as_ref().to_owned()),
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

    pub(crate) async fn send<T: DeserializeOwned, B: Serialize>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
        caller_headers: &[(&str, &str)],
    ) -> Result<T, ClientError> {
        let mut headers = Vec::with_capacity(caller_headers.len() + 1);
        let payload = if let Some(body) = body {
            if !caller_headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            {
                headers.push(("content-type", "application/json"));
            }
            headers.extend(caller_headers.iter().copied());
            serde_json::to_vec(body).map_err(|error| {
                invalid_request(format!("request serialization failed: {error}"))
            })?
        } else {
            headers.extend(caller_headers.iter().copied());
            Vec::new()
        };
        let (status, payload) = self.exchange(method, path, payload, &headers).await?;
        decode_envelope(status, &payload)
    }

    pub(crate) async fn send_text<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> Result<T, ClientError> {
        let (status, payload) = self
            .exchange(method, path, body.as_bytes().to_vec(), headers)
            .await?;
        decode_envelope(status, &payload)
    }

    pub(crate) async fn exchange(
        &self,
        method: Method,
        path: &str,
        payload: Vec<u8>,
        headers: &[(&str, &str)],
    ) -> Result<(StatusCode, Vec<u8>), ClientError> {
        validate_headers(headers)?;
        #[cfg(not(target_arch = "wasm32"))]
        return loopback::exchange(&self.endpoint, self.timeout, method, path, payload, headers)
            .await;

        #[cfg(target_arch = "wasm32")]
        return web::exchange(self.timeout, method, path, payload, headers).await;
    }
}

fn decode_envelope<T: DeserializeOwned>(
    status: StatusCode,
    payload: &[u8],
) -> Result<T, ClientError> {
    if !status.is_success() {
        return Err(api_error(status, payload));
    }
    serde_json::from_slice::<Envelope<T>>(payload)
        .map(|value| value.data)
        .map_err(|source| ClientError::Decode { source })
}

pub(crate) fn api_error(status: StatusCode, payload: &[u8]) -> ClientError {
    ClientError::Api {
        status,
        error: error_body(payload),
    }
}

/// Builds a [`ClientError`] for a failed underlying transport operation.
fn transport(error: impl std::fmt::Display) -> ClientError {
    ClientError::Transport {
        message: error.to_string(),
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

pub(crate) fn path_segment(value: &str) -> String {
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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::{Client, loopback, path_segment, validate_headers};
    use crate::{ClientError, SystemStatus};
    use http::Method;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::mpsc,
    };

    #[test]
    fn path_segment_preserves_unreserved_characters() {
        assert_eq!(path_segment("app-1_2.x~y"), "app-1_2.x~y");
        assert_eq!(path_segment(""), "");
    }

    #[test]
    fn path_segment_encodes_reserved_and_non_ascii_bytes() {
        assert_eq!(path_segment("a/b"), "a%2Fb");
        assert_eq!(path_segment("?#&="), "%3F%23%26%3D");
        assert_eq!(path_segment("é"), "%C3%A9");
        assert_eq!(path_segment("\r\n"), "%0D%0A");
        assert_eq!(path_segment(" "), "%20");
    }

    #[test]
    fn localhost_is_pinned_to_literal_loopback_addresses() {
        let loopback::Endpoint::Tcp {
            authority,
            addresses,
        } = loopback::tcp_endpoint("http://localhost:4321/").unwrap()
        else {
            panic!("localhost must produce a TCP endpoint");
        };
        assert_eq!(authority, "localhost:4321");
        assert_eq!(
            addresses,
            [
                "[::1]:4321".parse().unwrap(),
                "127.0.0.1:4321".parse().unwrap()
            ]
        );
    }

    #[test]
    fn invalid_header_values_are_rejected_without_echoing_them() {
        let error = validate_headers(&[("x-test", "secret\nvalue")]).unwrap_err();
        assert!(matches!(error, ClientError::Endpoint { .. }));
        assert!(!error.to_string().contains("secret"));
    }

    #[tokio::test]
    async fn bodyless_sends_forward_caller_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (wire_tx, mut wire_rx) = mpsc::channel(1);
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = Vec::new();
            loop {
                let mut chunk = [0u8; 1024];
                let read = socket.read(&mut chunk).await.unwrap();
                assert_ne!(read, 0, "client closed before sending request headers");
                buffer.extend_from_slice(&chunk[..read]);
                if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            wire_tx
                .send(String::from_utf8_lossy(&buffer).into_owned())
                .await
                .unwrap();
            let body = r#"{"data":{"status":"running","api_version":"v1","daemon_version":"0.1.0","instance_id":"i"}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        });
        let client = Client::tcp(&format!("http://{address}/")).unwrap();
        let status: SystemStatus = client
            .send::<_, ()>(
                Method::DELETE,
                "/api/v1/applications/app-1",
                None,
                &[("x-trace-id", "trace-42"), ("if-match", "\"7\"")],
            )
            .await
            .unwrap();
        server.await.unwrap();
        let wire = wire_rx.recv().await.unwrap();
        assert!(wire.starts_with("DELETE /api/v1/applications/app-1 HTTP/1.1\r\n"));
        let wire = wire.to_ascii_lowercase();
        assert!(wire.contains("\r\nx-trace-id: trace-42\r\n"));
        assert!(wire.contains("\r\nif-match: \"7\"\r\n"));
        assert!(!wire.contains("content-type"));
        assert_eq!(status.status, "running");
    }
}
