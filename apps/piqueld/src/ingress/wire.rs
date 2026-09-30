//! Sends HTTP requests through local socket files to Docker Engine and Caddy's
//! private admin API. The gateway uses these APIs to manage its container and
//! load routing configuration. Each request has a timeout and an 8 MiB response
//! limit so an unresponsive API cannot leave reconciliation waiting indefinitely.
//!
//! Reads are unrestricted. A mutating request can only be sent with an open
//! [`Journaled`] action, which commits intent before the request is made.
use crate::{operations::OperationError, store::JournalAction};
use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Method, Request, StatusCode, body::Bytes};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};
use tokio::net::UnixStream;

/// A non-success API response. The bounded body is kept for daemon logs;
/// public diagnostics record only the status.
#[derive(Debug, thiserror::Error)]
#[error("{path}: HTTP {status}: {body}")]
pub struct ResponseError {
    path: String,
    /// Status returned by Docker Engine or Caddy.
    pub status: StatusCode,
    body: String,
}

impl ResponseError {
    fn check(path: &str, status: StatusCode, body: &[u8]) -> Result<(), Self> {
        if status.is_success() {
            return Ok(());
        }
        Err(Self {
            path: path.to_owned(),
            status,
            body: String::from_utf8_lossy(body).chars().take(2048).collect(),
        })
    }
}

/// An open journal action for one daemon-owned gateway change.
pub(super) struct Journaled<'a> {
    store: &'a crate::store::Store,
    action: JournalAction,
    requests: AtomicU32,
}

impl Journaled<'_> {
    /// Commits `action_requested` for the next mutating request. The request
    /// must not be sent if this fails.
    pub(super) async fn request(&self) -> Result<()> {
        let request = self.requests.fetch_add(1, Ordering::SeqCst) + 1;
        self.store.action_request(&self.action, request).await?;
        Ok(())
    }

    /// Records the action's outcome, with a sanitized diagnostic on failure.
    pub(super) async fn finish<T>(self, result: Result<T>) -> Result<T> {
        let diagnostic = result
            .as_ref()
            .err()
            .map(OperationError::ingress_diagnostic);
        let recorded = self.store.finish_action(&self.action, diagnostic).await;
        match (result, recorded) {
            (Ok(value), Ok(())) => Ok(value),
            (Ok(_), Err(error)) => Err(error.into()),
            (Err(error), recorded) => {
                if let Err(journal) = recorded {
                    tracing::error!(error=?journal, "ingress action outcome could not be recorded");
                }
                Err(error)
            }
        }
    }
}

impl super::Ingress {
    /// Opens a daemon-scoped journal action. Callers decide from reads first and
    /// open an action only when a change is needed, so unchanged passes record
    /// no history.
    pub(super) async fn journal(&self, phase: &str, resource: &str) -> Result<Journaled<'_>> {
        Ok(Journaled {
            store: &self.store,
            action: self.store.begin_action(None, phase, Some(resource)).await?,
            requests: AtomicU32::new(0),
        })
    }
}

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
    async fn request(
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
    async fn json(&self, method: Method, path: &str, value: Option<&Value>) -> Result<Value> {
        let (status, body) = self.request(method, path, value).await?;
        ResponseError::check(path, status, &body)?;
        if body.is_empty() {
            Ok(Value::Null)
        } else {
            serde_json::from_slice(&body).context("decode ingress HTTP response")
        }
    }

    /// Reads and decodes a JSON resource.
    pub(super) async fn get(&self, path: &str) -> Result<Value> {
        self.json(Method::GET, path, None).await
    }

    /// Reads a resource's raw bytes, such as multiplexed Docker logs.
    pub(super) async fn read(&self, path: &str) -> Result<Vec<u8>> {
        let (status, body) = self.request(Method::GET, path, None).await?;
        ResponseError::check(path, status, &body)?;
        Ok(body)
    }

    /// Sends a mutating request after committing its intent to `journal`.
    pub(super) async fn send(
        &self,
        journal: &Journaled<'_>,
        method: Method,
        path: &str,
        value: Option<&Value>,
    ) -> Result<Value> {
        journal.request().await?;
        self.json(method, path, value).await
    }

    /// Simulates a change made outside the daemon, which the journal never sees.
    #[cfg(test)]
    pub(super) async fn external(
        &self,
        method: Method,
        path: &str,
        value: Option<&Value>,
    ) -> Result<Value> {
        self.json(method, path, value).await
    }

    /// Reads a Docker resource. A 404 means it does not exist; other failures
    /// remain errors so reconciliation cannot mistake an API outage for absence.
    pub(super) async fn inspect(&self, path: &str) -> Result<Option<Value>> {
        let (status, body) = self.request(Method::GET, path, None).await?;
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        ResponseError::check(path, status, &body)?;
        Ok(Some(
            serde_json::from_slice(&body).context("decode ingress inspection")?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use piqueld_core::observability::EventScope;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn journaled_requests_commit_intent_and_record_sanitized_failures() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("docker.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let engine = axum::Router::new()
            .fallback(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "private-engine-detail") });
        let server = tokio::spawn(async move { axum::serve(listener, engine).await });
        let store = Arc::new(
            crate::store::Store::open(directory.path().join("db"))
                .await
                .unwrap(),
        );
        let ingress =
            super::super::Ingress::new(true, &socket, directory.path(), Arc::clone(&store))
                .unwrap();
        let journal = ingress
            .journal("ingress_create_network", "edge")
            .await
            .unwrap();
        let result = ingress
            .docker
            .send(&journal, Method::POST, "/networks/create", None)
            .await;
        journal.finish(result).await.unwrap_err();
        server.abort();

        let events = store.events(None, None, 100).await.unwrap().items;
        let kinds: Vec<_> = events.iter().map(|event| event.kind.as_str()).collect();
        assert_eq!(
            kinds,
            ["action_started", "action_requested", "action_failed"]
        );
        let failed = &events[2];
        assert_eq!(failed.scope, EventScope::Daemon);
        assert_eq!(failed.error_code.as_deref(), Some("ingress_unavailable"));
        assert!(
            failed
                .diagnostic
                .as_ref()
                .unwrap()
                .causes
                .contains(&"Ingress API returned HTTP status 500".to_owned())
        );
        assert!(
            !serde_json::to_string(&events)
                .unwrap()
                .contains("private-engine-detail")
        );
    }

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
