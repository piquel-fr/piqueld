//! Bounded Unix HTTP transport shared by Docker lifecycle and Caddy control.
use anyhow::{Context, Result, ensure};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Method, Request, StatusCode, body::Bytes};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use std::{path::PathBuf, time::Duration};
use tokio::net::UnixStream;

pub(super) struct UnixApi(pub(super) PathBuf);

impl UnixApi {
    pub(super) async fn request(
        &self,
        method: Method,
        path: &str,
        value: Option<&Value>,
    ) -> Result<(StatusCode, Vec<u8>)> {
        tokio::time::timeout(Duration::from_secs(30), async {
            let stream = UnixStream::connect(&self.0)
                .await
                .with_context(|| format!("connect to {}", self.0.display()))?;
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
