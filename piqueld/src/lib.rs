mod config;
mod server;
mod services;

use anyhow::{Context, Result};
use clap::Parser;
use log::{info, trace};
use std::{fs, path::PathBuf};

use crate::{server::Server, services::git::GitHandle};
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

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let logger = Box::new(Logger::new(true, cli.verbose, true));
    logging::init(logger);
    trace!("Initialized logger");

    let config = config::ServerConfig::load(&cli.config_path)?;
    trace!("Loaded config");

    let data_dir = &config.data_dir;

    if !data_dir
        .try_exists()
        .with_context(|| format!("failed to detect data dir {data_dir:?}"))?
    {
        info!(
            "Data directory does not exist. Creating {:?}",
            config.data_dir
        );
        fs::create_dir_all(&config.data_dir)
            .with_context(|| format!("failed to create data dir {data_dir:?}"))?;
    }
    trace!("Setup data dir");

    let state: State = State {
        git: services::git::GitHandle::init(&config)
            .context("failed to initialize git service")?,
    };

    Ok(
        Server::new(state, (config.address, config.port), config.socket)
            .listen()
            .await?,
    )
}
