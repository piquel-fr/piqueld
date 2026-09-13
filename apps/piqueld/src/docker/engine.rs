use super::{Arc, BollardDocker, Docker, DockerError, ListNodesOptions, Path, ServiceSpec};
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode, body::Bytes, header};
use hyper_util::rt::TokioIo;
use std::time::Duration;
use tokio::net::UnixStream;

// A valid API request may be 2 MiB; Docker adds service metadata around that
// specification when it is inspected.
const MAX_SERVICE_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const SERVICE_REQUEST_TIMEOUT: Duration = super::DockerTimeout::Request.duration();

/// Bollard's per-request timeout, in seconds. Bollard only bounds a request up
/// to the response headers, which is why adapter calls additionally run under
/// the module-level deadline wrapper.
const BOLLARD_HEADER_TIMEOUT_SECS: u64 = 120;

#[derive(Debug)]
enum ServiceWireError {
    Public(DockerError),
    Response { status: StatusCode, body: Vec<u8> },
}

#[derive(Debug, thiserror::Error)]
#[error("Docker returned HTTP {status}: {message}")]
struct ServiceResponseDiagnostic {
    status: StatusCode,
    message: String,
}

impl ServiceWireError {
    fn sanitized(self, operation: &'static str) -> DockerError {
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

    fn is_not_found(&self) -> bool {
        matches!(
            self,
            Self::Response {
                status: StatusCode::NOT_FOUND,
                ..
            }
        )
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
    /// adapter translate that key and inspect Docker's exact update error.
    async fn service_request(
        &self,
        method: Method,
        path: &str,
        spec: Option<&ServiceSpec>,
    ) -> Result<Vec<u8>, ServiceWireError> {
        let body = if let Some(spec) = spec {
            let mut value = serde_json::to_value(spec).map_err(|source| {
                ServiceWireError::Public(DockerError::request(
                    "serialize service specification",
                    source,
                ))
            })?;
            Self::rename_swarm_healthcheck(&mut value, "HealthCheck", "Healthcheck");
            serde_json::to_vec(&value).map_err(|source| {
                ServiceWireError::Public(DockerError::request(
                    "serialize service specification",
                    source,
                ))
            })?
        } else {
            Vec::new()
        };

        let deadline = tokio::time::Instant::now() + SERVICE_REQUEST_TIMEOUT;
        let stream = tokio::time::timeout_at(deadline, UnixStream::connect(self.socket.as_ref()))
            .await
            .map_err(|source| {
                ServiceWireError::Public(DockerError::unavailable(
                    "connect to Docker Engine",
                    source,
                ))
            })?
            .map_err(|source| {
                ServiceWireError::Public(DockerError::unavailable(
                    "connect to Docker Engine",
                    source,
                ))
            })?;
        let (mut sender, connection) = match tokio::time::timeout_at(
            deadline,
            hyper::client::conn::http1::handshake(TokioIo::new(stream)),
        )
        .await
        {
            // Elapsed deadlines are unavailability, like every other timeout.
            Err(source) => {
                return Err(ServiceWireError::Public(DockerError::unavailable(
                    "open Docker service connection",
                    source,
                )));
            }
            Ok(Err(source)) => {
                return Err(ServiceWireError::Public(DockerError::request(
                    "open Docker service connection",
                    source,
                )));
            }
            Ok(Ok(parts)) => parts,
        };
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
                .map_err(|source| {
                    ServiceWireError::Public(DockerError::request(
                        "build Docker service request",
                        source,
                    ))
                })?;
            let response = sender.send_request(request).await.map_err(|source| {
                ServiceWireError::Public(DockerError::request(
                    "send Docker service request",
                    source,
                ))
            })?;
            Self::read_service_response(response).await
        })
        .await;
        drivers.abort_all();
        if let Some(Err(source)) = drivers.join_next().await
            && !source.is_cancelled()
        {
            return Err(ServiceWireError::Public(DockerError::request(
                "join Docker service connection",
                source,
            )));
        }
        result.map_err(|source| {
            ServiceWireError::Public(DockerError::unavailable("request Docker service", source))
        })?
    }

    /// Collects a bounded response while retaining transport failures.
    async fn read_service_response(
        response: hyper::Response<hyper::body::Incoming>,
    ) -> Result<Vec<u8>, ServiceWireError> {
        let status = response.status();
        let mut response = response.into_body();
        let mut body = Vec::new();
        while let Some(frame) = response.frame().await {
            let frame = frame.map_err(|source| {
                ServiceWireError::Public(DockerError::request(
                    "read Docker service response",
                    source,
                ))
            })?;
            let Ok(data) = frame.into_data() else {
                continue;
            };
            if body.len().saturating_add(data.len()) > MAX_SERVICE_RESPONSE_BYTES {
                return Err(ServiceWireError::Public(DockerError::Request(
                    "read Docker service response",
                )));
            }
            body.extend_from_slice(&data);
        }
        if status.is_success() {
            Ok(body)
        } else {
            Err(ServiceWireError::Response { status, body })
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

    pub(super) async fn create_service_wire(&self, spec: &ServiceSpec) -> Result<(), DockerError> {
        self.service_request(Method::POST, "/services/create", Some(spec))
            .await
            .map_err(|error| error.sanitized("create service"))
            .map(|_| ())
    }

    /// Updates a service with a bounded retry for Docker's exact transient
    /// optimistic-concurrency response. Every retry refreshes the current
    /// service version and resubmits the same desired specification.
    pub(super) async fn update_service_wire(
        &self,
        name: &str,
        mut version: u64,
        spec: &ServiceSpec,
    ) -> Result<(), DockerError> {
        let deadline = tokio::time::Instant::now() + SERVICE_REQUEST_TIMEOUT;
        loop {
            match tokio::time::timeout_at(
                deadline,
                self.service_request(
                    Method::POST,
                    // registryAuthFrom=spec is intentional: piqueld specs are
                    // auth-free, so Docker must not fall back to credentials
                    // from its own store.
                    &format!("/services/{name}/update?version={version}&registryAuthFrom=spec"),
                    Some(spec),
                ),
            )
            .await
            .map_err(|source| DockerError::unavailable("update service", source))?
            {
                Ok(_) => return Ok(()),
                Err(error)
                    if Self::update_out_of_sequence(&error)
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    let refreshed =
                        tokio::time::timeout_at(deadline, self.inspect_service_wire(name))
                            .await
                            .map_err(|source| {
                                DockerError::unavailable("refresh the service version", source)
                            })??;
                    version = refreshed
                        .and_then(|service| service.version)
                        .and_then(|value| value.index)
                        .ok_or(DockerError::Request("read refreshed service version"))?;
                }
                Err(error) => return Err(error.sanitized("update service")),
            }
        }
    }

    /// Returns whether Docker reported the one transient update conflict that
    /// is safe for the caller to retry with a refreshed service version.
    fn update_out_of_sequence(error: &ServiceWireError) -> bool {
        let ServiceWireError::Response { status, body } = error else {
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
        let nodes = Self::map_request(
            "list Swarm nodes",
            self.docker.list_nodes(None::<ListNodesOptions>).await,
        )?;
        if !Self::single_node_manager(&nodes) {
            return Err(DockerError::IncompatibleSwarm);
        }
        Ok(())
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

    /// Converts a Bollard request result while retaining the operation name.
    pub(super) fn map_request<T>(
        operation: &'static str,
        result: Result<T, bollard::errors::Error>,
    ) -> Result<T, DockerError> {
        result.map_err(|error| DockerError::request(operation, error))
    }
}

#[cfg(test)]
mod tests {
    use super::{BollardDocker, ServiceWireError, StatusCode};

    struct EngineStub {
        docker: BollardDocker,
        task: tokio::task::JoinHandle<()>,
        _directory: tempfile::TempDir,
    }

    impl EngineStub {
        fn respond(body: &'static str, declared_length: usize) -> Self {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let directory = tempfile::tempdir().unwrap();
            let socket = directory.path().join("docker.sock");
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let docker = BollardDocker::connect(&socket).unwrap();
            let task = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(stream.read_u8().await.unwrap());
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {declared_length}\r\nConnection: close\r\n\r\n{body}"
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            });
            Self {
                docker,
                task,
                _directory: directory,
            }
        }
    }

    #[tokio::test]
    async fn cancelling_service_request_closes_connection_driver() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("docker.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let docker = BollardDocker::connect(&socket).unwrap();
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
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("docker.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let docker = BollardDocker::connect(&socket).unwrap();
        drop(listener);
        std::fs::remove_file(socket).unwrap();
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

    #[test]
    fn update_retry_requires_the_exact_transient_daemon_response() {
        let transient = ServiceWireError::Response {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: br#"{"message":"rpc error: code = Unknown desc = update out of sequence"}"#
                .to_vec(),
        };
        let different_message = ServiceWireError::Response {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body:
                br#"{"message":"rpc error: code = Unknown desc = update out of sequence: other"}"#
                    .to_vec(),
        };
        let different_status = ServiceWireError::Response {
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
