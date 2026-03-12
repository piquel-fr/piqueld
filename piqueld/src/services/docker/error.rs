use thiserror::Error;

pub type Result<T> = std::result::Result<T, DockerError>;

#[derive(Debug, Error)]
pub enum DockerError {
    #[error("failed to connect to docker socket: {0}")]
    SocketConnectionError(#[source] bollard::errors::Error),
}
