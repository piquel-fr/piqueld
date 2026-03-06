use clap::Parser;
use std::path::PathBuf;

use piquelcore::{config::Config, ipc::server::Server};

use crate::config::DEFAULT_CONFIG_PATH;

mod config;

#[derive(Parser, Debug)]
#[command(name = "piquelctl")]
#[command(about = "CLI for piqueld", long_about = None)]
pub struct Cli {
    /// Custom path to configuration
    #[arg(long = "config", value_name = "path", global = true)]
    config_path: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let config_path: &PathBuf = match &cli.config_path {
        Some(path) => path,
        None => &PathBuf::from(DEFAULT_CONFIG_PATH),
    };

    let config = config::ServerConfig::load(config_path)?;

    Ok(Server::new(config.listen_addr, config.socket_path)
        .listen()
        .await?)
}
