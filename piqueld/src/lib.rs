mod config;
mod server;

use clap::Parser;
use log::{debug, info};
use std::{fs, path::PathBuf};

use crate::server::Server;
use piquel::{
    config::{Config, defaults},
    logging::{self, logger::Logger},
};

#[derive(Parser, Debug)]
#[command(name = "piquelctl")]
#[command(about = "CLI for piqueld", long_about = None)]
pub struct Cli {
    #[arg(short = 'v', long = "verbose", global = true)]
    pub verbose: bool,
    /// Custom path to configuration
    #[arg(long = "config",
        value_name = "path",
        global = true,
        default_value = defaults::SERVER_CONFIG_PATH
        )]
    config_path: PathBuf,
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let logger = Box::new(Logger::new(true, cli.verbose, true));
    logging::init(logger);

    let config = config::ServerConfig::load(&cli.config_path)?;

    if match config.data_dir.try_exists() {
        Ok(found) => found,
        Err(err) => return Err(format!("Failed to detect data directory: {err:#}").into()),
    } {
        info!(
            "Data directory does not exist. Creating {:?}",
            config.data_dir
        );
        match fs::create_dir_all(config.data_dir) {
            Ok(_) => debug!("Data directory created"),
            Err(err) => return Err(format!("Failed to create data directory: {err:#}").into()),
        };
    }

    Ok(Server::new((config.address, config.port), config.socket)
        .listen()
        .await?)
}
