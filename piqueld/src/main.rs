use clap::Parser;
use std::path::PathBuf;

use piquelcore::{
    config::{Config, defaults},
    ipc::server::Server,
};

mod config;

#[derive(Parser, Debug)]
#[command(name = "piquelctl")]
#[command(about = "CLI for piqueld", long_about = None)]
pub struct Cli {
    /// Custom path to configuration
    #[arg(long = "config",
        value_name = "path",
        global = true,
        default_value = defaults::SERVER_CONFIG_PATH
        )]
    config_path: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let config = config::ServerConfig::load(&cli.config_path)?;

    Ok(Server::new(config.listen_addr, config.socket_path)
        .listen()
        .await?)
}
