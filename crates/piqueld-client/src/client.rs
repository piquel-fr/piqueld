//! The typed API client with one request pipeline for every target.
//!
//! Endpoint methods, envelope decoding, and query building are shared. The
//! platform differences are confined to two small modules: [`loopback`]
//! speaks HTTP/1.1 over loopback TCP and Unix-domain sockets natively, and
//! [`web`] performs same-origin fetches in the browser.

use http::{Method, StatusCode};
use serde::{Serialize, de::DeserializeOwned};
use std::time::Duration;

use crate::{ClientError, Envelope, ErrorBody};

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
    use std::{path::PathBuf, time::Duration};
    use tokio::net::{TcpStream, UnixStream};
    use url::{Host, Url};

    use super::{ClientError, transport};

    #[derive(Clone, Debug)]
    pub(super) enum Endpoint {
        Tcp {
            authority: String,
            host: String,
            port: u16,
        },
        Unix(PathBuf),
    }

    /// Parses a loopback HTTP origin into a connectable endpoint.
    ///
    /// # Errors
    /// Returns [`ClientError::Endpoint`] when `base_url` is not a loopback
    /// HTTP origin.
    pub(super) fn tcp_endpoint(base_url: &str) -> Result<Endpoint, ClientError> {
        let url = Url::parse(base_url).map_err(|_| ClientError::Endpoint)?;
        if url.scheme() != "http"
            || !matches!(url.path(), "" | "/")
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some_and(|password| !password.is_empty())
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
        Ok(Endpoint::Tcp {
            authority,
            host,
            port,
        })
    }

    /// Sends one request and returns the status plus the collected body.
    pub(super) async fn exchange(
        endpoint: &Endpoint,
        timeout: Duration,
        method: Method,
        path: &str,
        payload: Vec<u8>,
        headers: &[(&str, &str)],
    ) -> Result<(StatusCode, Vec<u8>), ClientError> {
        let response = tokio::time::timeout(
            timeout,
            send_request(endpoint, method, path, payload, headers),
        )
        .await
        .map_err(|_| transport("request timed out"))??;
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(transport)?
            .to_bytes();
        Ok((status, body.to_vec()))
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
            .body(Full::new(Bytes::from(payload)))
            .map_err(|_| ClientError::Endpoint)?;
        match endpoint {
            Endpoint::Tcp { host, port, .. } => {
                speak(TcpStream::connect((host.as_str(), *port)).await, request).await
            }
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
        tokio::spawn(async move {
            let _ = connection.await;
        });
        sender.send_request(request).await.map_err(transport)
    }
}

#[cfg(target_arch = "wasm32")]
mod web {
    //! Browser transport: same-origin fetches via `gloo-net`.

    use gloo_net::http::{Request, RequestBuilder};
    use http::{Method, StatusCode};
    use std::time::Duration;
    use web_sys::{RequestCache, RequestCredentials, RequestMode};

    use super::{ClientError, transport};

    /// Sends one same-origin request and returns the status plus body bytes.
    pub(super) async fn exchange(
        timeout: Duration,
        method: Method,
        path: &str,
        payload: Vec<u8>,
        headers: &[(&str, &str)],
    ) -> Result<(StatusCode, Vec<u8>), ClientError> {
        // Browser fetches are cancelled by navigation and report network
        // failures promptly. Polling adds its own bounded cadence, so the
        // native per-request timer has no role here.
        let _ = timeout;
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
        .mode(RequestMode::SameOrigin);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = if payload.is_empty() {
            request.send().await
        } else {
            // JSON and TOML payloads are always UTF-8.
            let body = String::from_utf8(payload).map_err(|_| ClientError::Decode)?;
            request.body(body).map_err(transport)?.send().await
        }
        .map_err(transport)?;
        let status =
            StatusCode::from_u16(response.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = response.text().await.map_err(transport)?.into_bytes();
        Ok((status, body))
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
    #[cfg(not(target_arch = "wasm32"))]
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
        headers: &[(&str, &str)],
    ) -> Result<T, ClientError> {
        let mut headers = headers.to_vec();
        let payload = match body {
            Some(body) => {
                headers.push(("content-type", "application/json"));
                serde_json::to_vec(body).map_err(|_| ClientError::Decode)?
            }
            None => Vec::new(),
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
        .map_err(|_| ClientError::Decode)
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
