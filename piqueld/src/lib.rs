mod config;
mod git;
mod server;

use clap::Parser;
use log::info;
use std::{fs, path::PathBuf};

use crate::{git::GitHandle, server::Server};
use piquel::{
    config::{Config, defaults},
    logging::{self, logger::Logger},
};

#[derive(Parser, Debug)]
#[command(name = "piquelctl")]
#[command(about = "CLI for piqueld", long_about = None)]
struct Cli {
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

/// This is the mains struct of the daemon.
/// It stores all the state, logic and configuration of the application.
/// An instance of this is created by main and lent to all consumers.
///
/// Consumers are objects that receive external information. They include:
/// - TCP/UDS server listening for piquelctl commands
/// - Webhook listener (WIP)
/// - Cron scheduler (WIP)
pub struct State {
    pub git: GitHandle,
}

pub async fn run() -> piquel::Result<()> {
    let cli = Cli::parse();
    let logger = Box::new(Logger::new(true, cli.verbose, true));
    logging::init(logger);

    let config = config::ServerConfig::load(&cli.config_path)?;

    if match config.data_dir.try_exists() {
        Ok(found) => !found,
        Err(err) => return Err(format!("Failed to detect data directory: {err:#}").into()),
    } {
        info!(
            "Data directory does not exist. Creating {:?}",
            config.data_dir
        );
        match fs::create_dir_all(&config.data_dir) {
            Ok(_) => info!("Data directory created"),
            Err(err) => return Err(format!("Failed to create data directory: {err:#}").into()),
        };
    }

    let state: State = State {
        git: git::new_git_service(&config),
    };

    Ok(
        Server::new(state, (config.address, config.port), config.socket)
            .listen()
            .await?,
    )
}
