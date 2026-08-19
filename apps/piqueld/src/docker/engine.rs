use super::{Arc, BollardDocker, Docker, DockerError, ListNodesOptions, Path};

impl BollardDocker {
    /// Opens one shared, cheaply cloneable Bollard connection handle.
    ///
    /// # Errors
    /// Returns a sanitized unavailable error for invalid/non-Unix socket paths.
    pub fn connect(socket: &Path) -> Result<Self, DockerError> {
        let socket = socket
            .to_str()
            .ok_or(DockerError::Unavailable("connect to Docker Engine"))?;
        Docker::connect_with_unix(socket, 120, bollard::API_DEFAULT_VERSION)
            .map(|docker| Self {
                docker: Arc::new(docker),
            })
            .map_err(|error| DockerError::unavailable("connect to Docker Engine", error))
    }

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
