//! Bounded Unix HTTP transport shared by Docker lifecycle and Caddy control.
use anyhow::{Context, Result, ensure};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Method, Request, StatusCode, body::Bytes};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use std::{path::PathBuf, time::Duration};
use tokio::net::UnixStream;

pub(super) struct UnixApi {
    socket: PathBuf,
    timeout: Duration,
}

impl UnixApi {
    /// The caller supplies the operation budget; this covers connect and the full body.
    pub(super) fn new(socket: PathBuf, timeout: Duration) -> Self {
        Self { socket, timeout }
    }

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
