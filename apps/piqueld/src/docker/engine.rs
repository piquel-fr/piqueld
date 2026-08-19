use super::{Arc, BollardDocker, Docker, DockerError, ListNodesOptions, Path, ServiceSpec};
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode, body::Bytes, header};
use hyper_util::rt::TokioIo;
use std::time::Duration;
use tokio::net::UnixStream;

const MAX_SERVICE_RESPONSE_BYTES: usize = 1024 * 1024;
const SERVICE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
enum ServiceWireError {
    Public(DockerError),
    Response { status: StatusCode, body: Vec<u8> },
}

impl ServiceWireError {
    fn sanitized(self, operation: &'static str) -> DockerError {
        match self {
            Self::Public(error) => error,
            Self::Response { .. } => DockerError::Request(operation),
        }
    }
}

impl BollardDocker {
    /// Returns a cloneable Bollard handle for build operations.
    #[must_use]
    pub fn client(&self) -> Docker {
        self.docker.as_ref().clone()
    }

    /// Opens one shared, cheaply cloneable Bollard connection handle.
    ///
    /// # Errors
    /// Returns a sanitized unavailable error for invalid/non-Unix socket paths.
    pub fn connect(socket: &Path) -> Result<Self, DockerError> {
        let socket_path = socket.to_path_buf();
        let socket = socket_path
            .to_str()
            .ok_or(DockerError::Unavailable("connect to Docker Engine"))?;
        Docker::connect_with_unix(socket, 120, bollard::API_DEFAULT_VERSION)
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
            let mut value = serde_json::to_value(spec).map_err(|_| {
                ServiceWireError::Public(DockerError::Request("serialize service specification"))
            })?;
            Self::rename_swarm_healthcheck(&mut value, "HealthCheck", "Healthcheck");
            serde_json::to_vec(&value).map_err(|_| {
                ServiceWireError::Public(DockerError::Request("serialize service specification"))
            })?
        } else {
            Vec::new()
        };

        let deadline = tokio::time::Instant::now() + SERVICE_REQUEST_TIMEOUT;
        let stream = tokio::time::timeout_at(deadline, UnixStream::connect(self.socket.as_ref()))
            .await
            .map_err(|_| {
                ServiceWireError::Public(DockerError::Unavailable("connect to Docker Engine"))
            })?
            .map_err(|_| {
                ServiceWireError::Public(DockerError::Unavailable("connect to Docker Engine"))
            })?;
        let (mut sender, connection) = tokio::time::timeout_at(
            deadline,
            hyper::client::conn::http1::handshake(TokioIo::new(stream)),
        )
        .await
        .map_err(|_| {
            ServiceWireError::Public(DockerError::Request("open Docker service connection"))
        })?
        .map_err(|_| {
            ServiceWireError::Public(DockerError::Request("open Docker service connection"))
        })?;
        // Hyper returns a connection driver separately from the request sender;
        // it must run concurrently for the sender to make progress. Always
        // abort and join it after the request so no driver survives a timeout.
        let driver = tokio::spawn(async move {
            let _ = connection.await;
        });
        let result = tokio::time::timeout_at(deadline, async {
            let request = Request::builder()
                .method(method)
                .uri(path)
                .header(header::HOST, "localhost")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::CONNECTION, "close")
                .body(Full::new(Bytes::from(body)))
                .map_err(|_| {
                    ServiceWireError::Public(DockerError::Request("build Docker service request"))
                })?;
            let response = sender.send_request(request).await.map_err(|_| {
                ServiceWireError::Public(DockerError::Request("send Docker service request"))
            })?;
            let status = response.status();
            let mut response = response.into_body();
            let mut body = Vec::new();
            while let Some(frame) = response.frame().await {
                let frame = frame.map_err(|_| {
                    ServiceWireError::Public(DockerError::Request("read Docker service response"))
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
        })
        .await;
        driver.abort();
        let _ = driver.await;
        result.map_err(|_| {
            ServiceWireError::Public(DockerError::Unavailable("request Docker service"))
        })?
    }

    /// Inspects the complete service representation, restoring Bollard's
    /// typed health-check field after Docker's `Healthcheck` response key.
    pub(super) async fn inspect_service_wire(
        &self,
        identifier: &str,
    ) -> Result<bollard::models::Service, DockerError> {
        let bytes = self
            .service_request(Method::GET, &format!("/services/{identifier}"), None)
            .await
            .map_err(|error| error.sanitized("inspect service"))?;
        let mut value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|_| DockerError::Request("decode service response"))?;
        Self::rename_swarm_healthcheck(&mut value, "Healthcheck", "HealthCheck");
        serde_json::from_value(value).map_err(|_| DockerError::Request("decode service response"))
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
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            match self
                .service_request(
                    Method::POST,
                    &format!("/services/{name}/update?version={version}&registryAuthFrom=spec"),
                    Some(spec),
                )
                .await
            {
                Ok(_) => return Ok(()),
                Err(error)
                    if Self::update_out_of_sequence(&error)
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    version = self
                        .inspect_service_wire(name)
                        .await?
                        .version
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
        let spec = if value.get("Spec").is_some() {
            value.get_mut("Spec").expect("checked service spec")
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
