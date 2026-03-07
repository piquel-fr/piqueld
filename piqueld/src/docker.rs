use bollard::{Docker, errors::Error, secret::SwarmInitRequest};
use log::{debug, error, info};

const PREFIX: &str = "[Docker]";

pub async fn init() -> Result<(), Error> {
    let docker = match Docker::connect_with_socket_defaults() {
        Ok(docker) => docker,
        Err(err) => {
            error!("{PREFIX} Failed to connect to socket: {err:#}");
            return Err(err);
        }
    };
    info!("{PREFIX} Connected docker socket");

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
    Ok(())
}
