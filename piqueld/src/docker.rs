use bollard::{Docker, errors::Error};
use log::debug;

pub async fn init() -> Result<(), Error> {
    let docker = Docker::connect_with_socket_defaults()?;
    let info = docker.version().await?;
    debug!("Version: {info:?}");

    Ok(())
}
