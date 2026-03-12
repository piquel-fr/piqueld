use crate::config::ServerConfig;
use bollard::{Docker, errors::Error, secret::SwarmInitRequest};
use log::{debug, info, trace};

pub mod error;
use error::{DockerError, Result};
use piquelmacros::service;

const PREFIX: &str = "[Docker]";

struct DockerService {
    docker: Docker,
}

#[service(error = DockerError)]
impl DockerService {
    async fn init(_config: &ServerConfig) -> Result<Self> {
        let docker = Docker::connect_with_socket_defaults()
            .map_err(|source| DockerError::SocketConnectionError(source))?;
        info!("{PREFIX} Connected docker socket");

        // SWARM
        match docker.inspect_swarm().await {
            Ok(_) => trace!("{PREFIX} This is a Swarm Node"),
            Err(Error::DockerResponseServerError {
                status_code: _,
                message: _,
            }) => {
                info!("{PREFIX} This is not a Swarm Node. Attempting to initialise one...");
                let msg = docker
                    .init_swarm(SwarmInitRequest::default())
                    .await
                    .map_err(|err| DockerError::SwarmInitializationError(err))?;
                info!("{PREFIX} Successfully created swarm: {msg}");
            }
            Err(err) => return Err(DockerError::SwarmDetectionError(err)),
        }

        info!("{PREFIX} Initialised Docker service");
        Ok(DockerService { docker })
    }
}
