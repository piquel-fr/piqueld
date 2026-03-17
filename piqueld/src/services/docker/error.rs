use thiserror::Error;

pub type Result<T> = std::result::Result<T, DockerError>;

#[derive(Debug, Error)]
pub enum DockerError {
    #[error("failed to connect to docker socket: {0}")]
    SocketConnectionError(#[source] bollard::errors::Error),

    #[error("failed to detect docker swarm: {0}")]
    SwarmDetectionError(#[source] bollard::errors::Error),
    #[error("failed to initialize docker swarm: {0}")]
    SwarmInitializationError(#[source] bollard::errors::Error),

    #[error("docker swarm did not return a spec")]
    NoSwarmSpecError,
    #[error("docker swarm does not have a role")]
    NoSwarmRoleError,

    #[error("docker swarm node is not a manager")]
    NotManagerNode,

    #[error(transparent)]
    DockerError(#[from] bollard::errors::Error),
}
