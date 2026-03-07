mod config;
mod server;
mod git;

use clap::Parser;
use std::path::PathBuf;

use crate::server::Server;
use piquelcore::{
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
    let config = config::ServerConfig::load(&cli.config_path)?;

    let logger = Box::new(Logger::new(true, cli.verbose, true));
    logging::init(logger)?;

    Ok(Server::new((config.address, config.port), config.socket)
        .listen()
        .await?)
}
