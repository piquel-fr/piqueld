use super::{
    Arc, BollardDocker, BollardResult, Docker, DockerError, ListNodesOptions, Path, ResourceKind,
    ServiceSpec,
};
use futures_util::TryStreamExt as _;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode, body::Bytes, header};
use hyper_util::rt::TokioIo;
use std::time::Duration;
use tokio::net::UnixStream;

// A valid API request may be 2 MiB; Docker adds service metadata around that
// specification when it is inspected.
const MAX_SERVICE_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// Complete budget for one raw service request, from connect to last byte.
const SERVICE_REQUEST_TIMEOUT: Duration = super::DockerTimeout::Request.duration();

/// Bollard's per-request timeout, in seconds. Bollard only bounds a request up
/// to the response headers, which is why adapter calls additionally run under
/// the module-level deadline wrapper.
const BOLLARD_HEADER_TIMEOUT_SECS: u64 = 120;

#[derive(Debug)]
/// Failures of a raw Engine request, before they are classified publicly.
pub(super) enum WireError {
    /// Transport or local failures that already carry a public classification.
    Public(DockerError),
    /// A non-success HTTP response, kept raw so callers can match exact
    /// Engine errors (not found, update out of sequence) before sanitizing.
    Response { status: StatusCode, body: Vec<u8> },
}

#[derive(Debug, thiserror::Error)]
#[error("Docker returned HTTP {status}: {message}")]
/// Diagnostic source for an Engine error response, with a control-free,
/// length-bounded message.
struct ServiceResponseDiagnostic {
    status: StatusCode,
    message: String,
}

impl From<DockerError> for WireError {
    fn from(error: DockerError) -> Self {
        Self::Public(error)
    }
}

impl WireError {
    /// Converts the failure into a [`DockerError`] labelled with `operation`.
    ///
    /// Engine response bodies become a [`ServiceResponseDiagnostic`] source
    /// with control characters stripped and at most 2048 characters kept.
    pub(super) fn sanitized(self, operation: &'static str) -> DockerError {
        match self {
            Self::Public(error) => error,
            Self::Response { status, body } => {
                let message = String::from_utf8_lossy(&body)
                    .chars()
                    .filter(|character| !character.is_control())
                    .take(2_048)
                    .collect();
                DockerError::RequestSource {
                    operation,
                    source: Box::new(ServiceResponseDiagnostic { status, message }),
                }
            }
        }
    }

    /// Returns whether Docker answered with HTTP 404.
    pub(super) fn is_not_found(&self) -> bool {
        self.is_status(StatusCode::NOT_FOUND)
    }

    /// Returns whether Docker answered with `status`.
    pub(super) fn is_status(&self, status: StatusCode) -> bool {
        matches!(self, Self::Response { status: actual, .. } if *actual == status)
    }
}

impl BollardDocker {
    /// Opens one shared, cheaply cloneable Bollard connection handle.
    ///
    /// # Errors
    /// Returns a sanitized unavailable error for invalid/non-Unix socket paths.
    pub fn connect(socket: &Path) -> Result<Self, DockerError> {
        let socket_path = socket.to_path_buf();
        let socket = socket_path
            .to_str()
            .ok_or(DockerError::Unavailable("connect to Docker Engine"))?;
        Docker::connect_with_unix(
            socket,
            BOLLARD_HEADER_TIMEOUT_SECS,
            bollard::API_DEFAULT_VERSION,
        )
        .map(|docker| Self {
            docker: Arc::new(docker),
            socket: Arc::from(socket_path),
        })
        .map_err(|error| DockerError::unavailable("connect to Docker Engine", error))
    }

    /// Performs one raw service request through Docker's Unix socket.
    ///
    /// Bollard's service model uses a different health-check key spelling than
    /// the Swarm API. Keeping the request and response bytes here lets the
    /// adapter translate that key and inspect Docker's exact update error:
    /// `spec` (if any) is serialized with `HealthCheck` renamed to `Healthcheck`,
    /// then sent by [`Self::engine_request`].
    async fn service_request(
        &self,
        method: Method,
        path: &str,
        spec: Option<&ServiceSpec>,
    ) -> Result<Vec<u8>, WireError> {
        let body = match spec {
            Some(spec) => serde_json::to_value(spec)
                .and_then(|mut value| {
                    Self::rename_swarm_healthcheck(&mut value, "HealthCheck", "Healthcheck");
                    serde_json::to_vec(&value)
                })
                .map_err(|source| {
                    DockerError::request("serialize service specification", source)
                })?,
            None => Vec::new(),
        };
        self.engine_request(method, path, body).await
    }

    /// Performs one raw Engine request whose response is at most
    /// [`MAX_SERVICE_RESPONSE_BYTES`]; see [`Self::engine_exchange`].
    pub(super) async fn engine_request(
        &self,
        method: Method,
        path: &str,
        body: Vec<u8>,
    ) -> Result<Vec<u8>, WireError> {
        self.engine_exchange(method, path, body, Self::read_service_response)
            .await
    }

    /// Lists `T`s with a raw `GET` of `path`, decoding the JSON array as it
    /// arrives. Fields `T` leaves out, such as config payloads or service
    /// environments, are skipped rather than held, so a listing needs no
    /// size bound: memory follows the largest `T`, not the whole response.
    pub(super) async fn engine_list<T: serde::de::DeserializeOwned + Send + 'static>(
        &self,
        path: &str,
    ) -> Result<Vec<T>, WireError> {
        self.engine_exchange(Method::GET, path, Vec::new(), async |response| {
            if !response.status().is_success() {
                return Self::read_service_response(response)
                    .await
                    .map(|_| Vec::new());
            }
            let data = http_body_util::BodyStream::new(response.into_body())
                .try_filter_map(|frame| std::future::ready(Ok(frame.into_data().ok())))
                .map_err(std::io::Error::other);
            let reader = tokio_util::io::SyncIoBridge::new(tokio_util::io::StreamReader::new(
                Box::pin(data),
            ));
            let decoded = tokio::task::spawn_blocking(move || {
                serde_json::from_reader(std::io::BufReader::new(reader))
            })
            .await
            .map_err(|source| DockerError::request("decode Docker listing", source))?;
            Ok(decoded.map_err(|source| DockerError::request("decode Docker listing", source))?)
        })
        .await
    }

    /// Performs one raw Engine request with a JSON `body` through Docker's
    /// Unix socket, for endpoints Bollard lacks or misrepresents.
    ///
    /// 1. Connect and perform an HTTP/1 handshake on the Unix socket.
    /// 2. Send one `Connection: close` request and pass the response to
    ///    `read`.
    ///
    /// All steps share one [`SERVICE_REQUEST_TIMEOUT`] deadline; elapsed
    /// deadlines surface as unavailability.
    async fn engine_exchange<T>(
        &self,
        method: Method,
        path: &str,
        body: Vec<u8>,
        read: impl AsyncFnOnce(hyper::Response<hyper::body::Incoming>) -> Result<T, WireError>,
    ) -> Result<T, WireError> {
        let deadline = tokio::time::Instant::now() + SERVICE_REQUEST_TIMEOUT;
        let connect = "connect to Docker Engine";
        let stream = tokio::time::timeout_at(deadline, UnixStream::connect(self.socket.as_ref()))
            .await
            .map_err(|source| DockerError::unavailable(connect, source))?
            .map_err(|source| DockerError::unavailable(connect, source))?;
        let open = "open Docker service connection";
        // Elapsed deadlines are unavailability, like every other timeout.
        let (mut sender, connection) = tokio::time::timeout_at(
            deadline,
            hyper::client::conn::http1::handshake(TokioIo::new(stream)),
        )
        .await
        .map_err(|source| DockerError::unavailable(open, source))?
        .map_err(|source| DockerError::request(open, source))?;
        // Hyper returns a connection driver separately from the request sender;
        // it must run concurrently for the sender to make progress. Always
        // abort and join it after the request. JoinSet also aborts on drop when
        // an outer deadline or caller cancels this future.
        let mut drivers = tokio::task::JoinSet::new();
        drivers.spawn(async move {
            if let Err(source) = connection.await {
                tracing::debug!(error = ?source, "Docker service connection driver failed");
            }
        });
        let result = tokio::time::timeout_at(deadline, async {
            let request = Request::builder()
                .method(method)
                .uri(path)
                // Docker's Unix-socket HTTP endpoint still requires a Host
                // header; localhost is the conventional placeholder.
                .header(header::HOST, "localhost")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::CONNECTION, "close")
                .body(Full::new(Bytes::from(body)))
                .map_err(|source| DockerError::request("build Docker service request", source))?;
            let response = sender
                .send_request(request)
                .await
                .map_err(|source| DockerError::request("send Docker service request", source))?;
            read(response).await
        })
        .await;
        drivers.abort_all();
        if let Some(Err(source)) = drivers.join_next().await
            && !source.is_cancelled()
        {
            return Err(DockerError::request("join Docker service connection", source).into());
        }
        result.map_err(|source| DockerError::unavailable("request Docker service", source))?
    }

    /// Collects a bounded response while retaining transport failures.
    ///
    /// Bodies larger than [`MAX_SERVICE_RESPONSE_BYTES`] are rejected, and
    /// non-success statuses keep their body for exact error matching.
    async fn read_service_response(
        response: hyper::Response<hyper::body::Incoming>,
    ) -> Result<Vec<u8>, WireError> {
        let status = response.status();
        let mut response = response.into_body();
        let mut body = Vec::new();
        while let Some(frame) = response.frame().await {
            let frame = frame
                .map_err(|source| DockerError::request("read Docker service response", source))?;
            let Ok(data) = frame.into_data() else {
                continue;
            };
            if body.len().saturating_add(data.len()) > MAX_SERVICE_RESPONSE_BYTES {
                return Err(DockerError::Request("read Docker service response").into());
            }
            body.extend_from_slice(&data);
        }
        if status.is_success() {
            Ok(body)
        } else {
            Err(WireError::Response { status, body })
        }
    }

    /// Inspects the complete service representation, restoring Bollard's
    /// typed health-check field after Docker's `Healthcheck` response key.
    ///
    /// Returns `None` when the service no longer exists.
    pub(super) async fn inspect_service_wire(
        &self,
        identifier: &str,
    ) -> Result<Option<bollard::models::Service>, DockerError> {
        let bytes = match self
            .service_request(Method::GET, &format!("/services/{identifier}"), None)
            .await
        {
            Ok(bytes) => bytes,
            Err(error) if error.is_not_found() => return Ok(None),
            Err(error) => return Err(error.sanitized("inspect service")),
        };
        let mut value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|source| DockerError::request("decode service response", source))?;
        Self::rename_swarm_healthcheck(&mut value, "Healthcheck", "HealthCheck");
        serde_json::from_value(value)
            .map(Some)
            .map_err(|source| DockerError::request("decode service response", source))
    }

    /// Creates a service from `spec` through the raw service endpoint.
    pub(super) async fn create_service_wire(&self, spec: &ServiceSpec) -> Result<(), DockerError> {
        self.service_request(Method::POST, "/services/create", Some(spec))
            .await
            .map_err(|error| error.sanitized("create service"))
            .map(|_| ())
    }

    /// Updates a service with a bounded retry for Docker's exact transient
    /// optimistic-concurrency response. Every retry refreshes the current
    /// service version and rechecks ownership before resubmitting the specification.
    /// The immutable service ID prevents a replacement with the same name being updated;
    /// a vanished, replaced, renamed, or foreign service is `OwnershipConflict`.
    pub(super) async fn update_service_wire(
        &self,
        id: &str,
        mut version: u64,
        spec: &ServiceSpec,
    ) -> Result<(), DockerError> {
        let deadline = tokio::time::Instant::now() + SERVICE_REQUEST_TIMEOUT;
        loop {
            match tokio::time::timeout_at(deadline, self.post_service_update(id, version, spec))
                .await
                .map_err(|source| DockerError::unavailable("update service", source))?
            {
                Ok(()) => return Ok(()),
                Err(error)
                    if Self::update_out_of_sequence(&error)
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    let refreshed =
                        tokio::time::timeout_at(deadline, self.inspect_service_wire(id))
                            .await
                            .map_err(|source| {
                                DockerError::unavailable("refresh the service version", source)
                            })??;
                    let refreshed = refreshed.ok_or(DockerError::OwnershipConflict)?;
                    let name = refreshed
                        .spec
                        .as_ref()
                        .and_then(|spec| spec.name.as_deref());
                    if refreshed.id.as_deref() != Some(id) || name != spec.name.as_deref() {
                        return Err(DockerError::OwnershipConflict);
                    }
                    Self::check_ownership(
                        refreshed
                            .spec
                            .as_ref()
                            .and_then(|spec| spec.labels.clone())
                            .unwrap_or_default(),
                        &spec
                            .labels
                            .clone()
                            .unwrap_or_default()
                            .into_iter()
                            .collect(),
                        ResourceKind::Service,
                        spec.name.as_deref().unwrap_or_default(),
                    )?;
                    version = refreshed
                        .version
                        .and_then(|value| value.index)
                        .ok_or(DockerError::Request("read refreshed service version"))?;
                }
                Err(error) => return Err(error.sanitized("update service")),
            }
        }
    }

    /// Submits `spec` once as service `id`'s update at `version`.
    pub(super) async fn post_service_update(
        &self,
        id: &str,
        version: u64,
        spec: &ServiceSpec,
    ) -> Result<(), WireError> {
        self.service_request(
            Method::POST,
            // registryAuthFrom=spec is intentional: piqueld specs are
            // auth-free, so Docker must not fall back to credentials
            // from its own store.
            &format!("/services/{id}/update?version={version}&registryAuthFrom=spec"),
            Some(spec),
        )
        .await
        .map(|_| ())
    }

    /// Returns whether Docker reported the one transient update conflict that
    /// is safe for the caller to retry with a refreshed service version.
    fn update_out_of_sequence(error: &WireError) -> bool {
        let WireError::Response { status, body } = error else {
            return false;
        };
        *status == StatusCode::INTERNAL_SERVER_ERROR
            && serde_json::from_slice::<serde_json::Value>(body)
                .ok()
                .and_then(|value| value.get("message")?.as_str().map(str::to_owned))
                .is_some_and(|message| {
                    message == "rpc error: code = Unknown desc = update out of sequence"
                })
    }

    /// Renames the health-check key in either a service spec or a service response.
    ///
    /// Accepts a bare spec or a service object with a `Spec` field; values
    /// without a container spec are left untouched.
    ///
    /// ```text
    /// {"TaskTemplate":{"ContainerSpec":{"HealthCheck":…}}}
    ///   → {"TaskTemplate":{"ContainerSpec":{"Healthcheck":…}}}
    /// ```
    fn rename_swarm_healthcheck(value: &mut serde_json::Value, from: &str, to: &str) {
        let spec = if let Some(spec) = value.get_mut("Spec") {
            spec
        } else {
            value
        };
        let Some(container) = spec
            .get_mut("TaskTemplate")
            .and_then(|task| task.get_mut("ContainerSpec"))
            .and_then(serde_json::Value::as_object_mut)
        else {
            return;
        };
        if let Some(healthcheck) = container.remove(from) {
            container.insert(to.to_owned(), healthcheck);
        }
    }

    /// Fetches the local nodes and rejects anything other than one ready,
    /// reachable, active manager.
    pub(super) async fn validate_single_node_manager(&self) -> Result<(), DockerError> {
        let nodes = self
            .docker
            .list_nodes(None::<ListNodesOptions>)
            .await
            .or_request("list Swarm nodes")?;
        if !Self::single_node_manager(&nodes) {
            return Err(DockerError::IncompatibleSwarm);
        }
        Ok(())
    }

    /// Reads the daemon's Swarm membership, labelling failures with `operation`.
    pub(super) async fn swarm_info(
        &self,
        operation: &'static str,
    ) -> Result<bollard::models::SwarmInfo, DockerError> {
        let info = self
            .docker
            .info()
            .await
            .map_err(|source| DockerError::unavailable(operation, source))?;
        Ok(info.swarm.unwrap_or_default())
    }

    /// Returns the local manager's immutable node ID, which pins services to this
    /// daemon. Topology is not checked here, so observation and removals keep
    /// working if another node joins; mutations check it through `ensure_swarm`.
    pub(super) async fn local_node_id(&self) -> Result<String, DockerError> {
        let swarm = self.swarm_info("inspect Docker Swarm state").await?;
        if swarm.control_available != Some(true) {
            return Err(DockerError::NotManager);
        }
        swarm
            .node_id
            .filter(|id| !id.is_empty())
            .ok_or(DockerError::Request("read local Swarm node identity"))
    }

    /// Returns whether Docker reports exactly one ready, reachable manager.
    pub(super) fn single_node_manager(nodes: &[bollard::models::Node]) -> bool {
        nodes.len() == 1
            && nodes[0].spec.as_ref().and_then(|spec| spec.role)
                == Some(bollard::models::NodeSpecRoleEnum::MANAGER)
            && nodes[0].spec.as_ref().and_then(|spec| spec.availability)
                == Some(bollard::models::NodeSpecAvailabilityEnum::ACTIVE)
            && nodes[0].status.as_ref().and_then(|status| status.state)
                == Some(bollard::models::NodeState::READY)
            && nodes[0]
                .manager_status
                .as_ref()
                .and_then(|status| status.reachability)
                == Some(bollard::models::Reachability::REACHABLE)
    }
}

#[cfg(test)]
mod tests {
    use super::{BollardDocker, StatusCode, WireError};

    struct EngineStub {
        docker: BollardDocker,
        task: tokio::task::JoinHandle<()>,
        _directory: tempfile::TempDir,
    }

    /// A client of the engine socket in a new directory, and the listener
    /// behind it.
    fn listen() -> (tempfile::TempDir, tokio::net::UnixListener, BollardDocker) {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("docker.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let docker = BollardDocker::connect(&socket).unwrap();
        (directory, listener, docker)
    }

    impl EngineStub {
        /// Serves one raw 200 response whose declared length may disagree with the body.
        fn respond(body: &'static str, declared_length: usize) -> Self {
            Self::serve(vec![(
                None,
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {declared_length}\r\nConnection: close\r\n\r\n{body}"
                ),
            )])
        }

        /// Serves JSON responses in order, asserting each request line.
        fn sequence(responses: Vec<(String, u16, serde_json::Value)>) -> Self {
            Self::serve(
                responses
                    .into_iter()
                    .map(|(request, status, body)| {
                        let body = body.to_string();
                        (
                            Some(request),
                            format!(
                                "HTTP/1.1 {status} fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            ),
                        )
                    })
                    .collect(),
            )
        }

        /// Answers one connection per exchange with its raw response, checking
        /// the request line when one is expected.
        fn serve(exchanges: Vec<(Option<String>, String)>) -> Self {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (directory, listener, docker) = listen();
            let task = tokio::spawn(async move {
                for (expected, response) in exchanges {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut headers = Vec::new();
                    while !headers.ends_with(b"\r\n\r\n") {
                        headers.push(stream.read_u8().await.unwrap());
                    }
                    let headers = String::from_utf8(headers).unwrap();
                    if let Some(expected) = expected {
                        assert_eq!(
                            headers.lines().next().unwrap(),
                            format!("{expected} HTTP/1.1")
                        );
                    }
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().unwrap())
                        })
                        .unwrap_or_default();
                    stream.read_exact(&mut vec![0; length]).await.unwrap();
                    stream.write_all(response.as_bytes()).await.unwrap();
                    stream.shutdown().await.unwrap();
                }
            });
            Self {
                docker,
                task,
                _directory: directory,
            }
        }
    }

    #[tokio::test]
    async fn service_update_retry_uses_id_and_rechecks_ownership() {
        use piqueld_core::resource::{
            APPLICATION_LABEL, INSTANCE_LABEL, MANAGED_LABEL, SERVICE_LABEL, SPEC_HASH_LABEL,
        };
        let app = piqueld_core::EnvironmentId::parse("app-update-race").unwrap();
        let name = piqueld_core::docker_resource_name(
            &app,
            piqueld_core::ResourceKind::Service,
            Some("web"),
        );
        let spec = bollard::models::ServiceSpec {
            name: Some(name.clone()),
            labels: Some(std::collections::HashMap::from([
                (MANAGED_LABEL.into(), "true".into()),
                (INSTANCE_LABEL.into(), "test-instance".into()),
                (APPLICATION_LABEL.into(), app.to_string()),
                (SERVICE_LABEL.into(), "web".into()),
                (SPEC_HASH_LABEL.into(), format!("sha256:{}", "a".repeat(64))),
            ])),
            ..Default::default()
        };
        for changed in ["none", "owner", "id", "name", "removed"] {
            let mut refreshed =
                serde_json::json!({"ID":"immutable-id", "Version":{"Index":42}, "Spec":spec});
            match changed {
                "owner" => refreshed["Spec"]["Labels"][INSTANCE_LABEL] = "another-instance".into(),
                "id" => refreshed["ID"] = "replacement-id".into(),
                "name" => refreshed["Spec"]["Name"] = "renamed-service".into(),
                _ => {}
            }
            let mut responses = vec![
                (
                    "POST /services/immutable-id/update?version=1&registryAuthFrom=spec".into(),
                    500,
                    serde_json::json!({"message":"rpc error: code = Unknown desc = update out of sequence"}),
                ),
                (
                    "GET /services/immutable-id".into(),
                    if changed == "removed" { 404 } else { 200 },
                    refreshed,
                ),
            ];
            if changed == "none" {
                responses.push((
                    "POST /services/immutable-id/update?version=42&registryAuthFrom=spec".into(),
                    200,
                    serde_json::json!({}),
                ));
            }
            let stub = EngineStub::sequence(responses);
            let result = stub
                .docker
                .update_service_wire("immutable-id", 1, &spec)
                .await;
            if changed == "none" {
                result.unwrap();
            } else {
                assert!(
                    matches!(result, Err(super::DockerError::OwnershipConflict)),
                    "{changed}: {result:?}"
                );
            }
            stub.task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn cancelling_service_request_closes_connection_driver() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (_directory, listener, docker) = listen();
        let request = tokio::spawn(async move { docker.inspect_service_wire("test").await });
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            headers.push(stream.read_u8().await.unwrap());
        }
        // Leave the body pending so the driver still owns an open connection.
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n{")
            .await
            .unwrap();
        tokio::task::yield_now().await;
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        let mut byte = [0];
        let read =
            tokio::time::timeout(std::time::Duration::from_secs(1), stream.read(&mut byte)).await;
        assert_eq!(read.expect("cancelled connection must close").unwrap(), 0);
    }

    #[tokio::test]
    async fn missing_engine_retains_io_cause_and_public_classification() {
        use std::error::Error;
        let (directory, listener, docker) = listen();
        drop(listener);
        std::fs::remove_file(directory.path().join("docker.sock")).unwrap();
        let error = docker.inspect_service_wire("test").await.unwrap_err();
        let source = error
            .source()
            .unwrap()
            .downcast_ref::<std::io::Error>()
            .unwrap();
        assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(
            crate::operations::OperationError::from(error).code(),
            "docker_unavailable"
        );
    }

    #[tokio::test]
    async fn malformed_service_response_retains_decode_cause() {
        use std::error::Error;
        let engine = EngineStub::respond("not JSON", 8);
        let error = engine
            .docker
            .inspect_service_wire("test")
            .await
            .unwrap_err();
        engine.task.await.unwrap();
        assert!(error.source().unwrap().is::<serde_json::Error>());
        assert_eq!(
            error.to_string(),
            "Docker request failed while decode service response"
        );
    }

    #[tokio::test]
    async fn truncated_service_body_retains_transport_cause() {
        use std::error::Error;
        let engine = EngineStub::respond("{", 20);
        let error = engine
            .docker
            .inspect_service_wire("test")
            .await
            .unwrap_err();
        engine.task.await.unwrap();
        assert!(error.source().unwrap().is::<hyper::Error>());
        assert_eq!(
            crate::operations::OperationError::from(error).code(),
            "docker_request_failed"
        );
    }

    /// A secret that another request created, then removed before the
    /// recheck, fails the request: Docker answered, so it is no outage.
    #[tokio::test]
    async fn a_secret_removed_during_creation_is_a_request_failure() {
        let response = |status: u16| {
            let body = r#"{"message":"fixture"}"#;
            (
                None,
                format!(
                    "HTTP/1.1 {status} fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                ),
            )
        };
        let stub = EngineStub::serve(vec![response(404), response(409), response(404)]);
        let error = stub
            .docker
            .provision_secret("secret", b"value", &std::collections::BTreeMap::new())
            .await
            .unwrap_err();
        stub.task.await.unwrap();
        assert_eq!(
            crate::operations::OperationError::from(error).code(),
            "docker_request_failed"
        );
    }

    #[test]
    fn update_retry_requires_the_exact_transient_daemon_response() {
        let transient = WireError::Response {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: br#"{"message":"rpc error: code = Unknown desc = update out of sequence"}"#
                .to_vec(),
        };
        let different_message = WireError::Response {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body:
                br#"{"message":"rpc error: code = Unknown desc = update out of sequence: other"}"#
                    .to_vec(),
        };
        let different_status = WireError::Response {
            status: StatusCode::CONFLICT,
            body: br#"{"message":"rpc error: code = Unknown desc = update out of sequence"}"#
                .to_vec(),
        };

        assert!(BollardDocker::update_out_of_sequence(&transient));
        assert!(!BollardDocker::update_out_of_sequence(&different_message));
        assert!(!BollardDocker::update_out_of_sequence(&different_status));
    }

    #[test]
    fn healthcheck_key_is_translated_at_the_docker_wire_boundary() {
        let mut value = serde_json::json!({
            "TaskTemplate": {"ContainerSpec": {"HealthCheck": {"Test": ["CMD", "true"]}}}
        });
        BollardDocker::rename_swarm_healthcheck(&mut value, "HealthCheck", "Healthcheck");
        let container = &value["TaskTemplate"]["ContainerSpec"];
        assert!(container.get("HealthCheck").is_none());
        assert!(container.get("Healthcheck").is_some());

        let mut response = serde_json::json!({"Spec": value});
        BollardDocker::rename_swarm_healthcheck(&mut response, "Healthcheck", "HealthCheck");
        let container = &response["Spec"]["TaskTemplate"]["ContainerSpec"];
        assert!(container.get("HealthCheck").is_some());
        assert!(container.get("Healthcheck").is_none());
    }
}
