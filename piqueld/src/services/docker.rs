use crate::config::ServerConfig;
use bollard::{Docker, errors::Error, secret::SwarmInitRequest};
use log::{debug, error, info};

pub mod error;
use error::{DockerError, Result};
use piquelmacros::service;

const PREFIX: &str = "[Docker]";

struct DockerService {
    docker: Docker,
}

#[service(error = DockerError)]
impl DockerService {
    async fn init(config: &ServerConfig) -> Result<Self> {
        let docker = Docker::connect_with_socket_defaults()
            .map_err(|source| DockerError::SocketConnectionError(source))?;
        info!("{PREFIX} Connected docker socket");

        // TODO: error handling & async
        // SWARM
        match docker.inspect_swarm().await {
            Ok(_) => debug!("{PREFIX} This is a Swarm Node"),
            Err(Error::DockerResponseServerError {
                status_code: _,
                message: _,
            }) => {
                info!("{PREFIX} This is not a Swarm Node. Attempting to initialise one...");
                match docker.init_swarm(SwarmInitRequest::default()).await {
                    Ok(msg) => info!("{PREFIX} Successfully created swarm: {msg}"),
                    Err(err) => {
                        error!("{PREFIX} Unable to initialise Swarm: {err:#}");
                        return Err(err);
                    }
                };
            }
            Err(err) => {
                error!("{PREFIX} Unable to detect Swarm: {err:#}");
                return Err(err);
            }
        }

        info!("{PREFIX} Initialised Docker service");
        Ok(DockerService { docker })
    }
}
