//! Sends HTTP requests through local socket files to Docker Engine and Caddy's
//! private admin API. The gateway uses these APIs to manage its container and
//! load routing configuration. Each request has a timeout and an 8 MiB response
//! limit so an unresponsive API cannot leave reconciliation waiting indefinitely.
use anyhow::{Context, Result, ensure};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Method, Request, StatusCode, body::Bytes};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use std::{path::PathBuf, time::Duration};
use tokio::net::UnixStream;

/// Client for one local HTTP API, identified by its Unix socket path.
/// Opens a fresh connection for each request; constructing it performs no I/O.
pub(super) struct UnixApi {
    socket: PathBuf,
    timeout: Duration,
}

impl UnixApi {
    /// Uses the caller's timeout for the entire request, including connecting
    /// and reading the response. Ingress supplies the global Docker request timeout.
    pub(super) fn new(socket: PathBuf, timeout: Duration) -> Self {
        Self { socket, timeout }
    }

    /// Sends optional JSON and returns the HTTP status and raw response bytes.
    /// Callers interpret the status; Docker logs also need the undecoded body.
    pub(super) async fn request(
        &self,
        method: Method,
        path: &str,
        value: Option<&Value>,
    ) -> Result<(StatusCode, Vec<u8>)> {
        tokio::time::timeout(self.timeout, async {
            let stream = UnixStream::connect(&self.socket)
                .await
                .with_context(|| format!("connect to {}", self.socket.display()))?;
            let (mut sender, connection) =
                hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
            let mut drivers = tokio::task::JoinSet::new();
            drivers.spawn(async move {
                if let Err(error) = connection.await {
                    tracing::debug!(?error, "ingress HTTP connection ended");
                }
            });
            let bytes = value
                .map(serde_json::to_vec)
                .transpose()?
                .unwrap_or_default();
            let request = Request::builder()
                .method(method)
                .uri(path)
                .header("Host", "localhost")
                .header("Content-Type", "application/json")
                .header("Connection", "close")
                .body(Full::new(Bytes::from(bytes)))?;
            let response = sender.send_request(request).await?;
            let status = response.status();
            let bytes = Limited::new(response.into_body(), 8 * 1024 * 1024)
                .collect()
                .await
                .map_err(|error| anyhow::anyhow!("read bounded ingress response: {error}"))?
                .to_bytes();
            Ok((status, bytes.to_vec()))
        })
        .await
        .context("ingress HTTP request timed out")?
    }

    /// Requires a successful HTTP status and decodes JSON, or returns `Null`
    /// for an empty response (as returned by Docker start/stop operations).
    pub(super) async fn json(
        &self,
        method: Method,
        path: &str,
        value: Option<&Value>,
    ) -> Result<Value> {
        let (status, body) = self.request(method, path, value).await?;
        ensure!(
            status.is_success(),
            "{path}: HTTP {status}: {}",
            String::from_utf8_lossy(&body)
                .chars()
                .take(2048)
                .collect::<String>()
        );
        if body.is_empty() {
            Ok(Value::Null)
        } else {
            serde_json::from_slice(&body).context("decode ingress HTTP response")
        }
    }

    /// Reads a Docker resource. A 404 means it does not exist; other failures
    /// remain errors so reconciliation cannot mistake an API outage for absence.
    pub(super) async fn inspect(&self, path: &str) -> Result<Option<Value>> {
        let (status, body) = self.request(Method::GET, path, None).await?;
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        ensure!(
            status.is_success(),
            "{path}: HTTP {status}: {}",
            String::from_utf8_lossy(&body)
                .chars()
                .take(2048)
                .collect::<String>()
        );
        Ok(Some(
            serde_json::from_slice(&body).context("decode ingress inspection")?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn supplied_budget_covers_a_stalled_response_body() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("api.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let mut server = tokio::task::JoinSet::new();
        server.spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.read_exact(&mut [0; 1]).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nx")
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let client = UnixApi::new(socket, Duration::from_millis(50));
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            client.request(Method::GET, "/", None),
        )
        .await
        .expect("the supplied 50ms budget must override the usual request budget")
        .unwrap_err();
        assert!(
            error
                .chain()
                .any(<dyn std::error::Error>::is::<tokio::time::error::Elapsed>)
        );
    }
}
