use std::path::PathBuf;
use std::{io, panic};

use log::{error, info};
use piquel::{
    config::{Config, defaults},
    ipc::{
        client::Client,
        message::{Command, Response},
    },
    logging::{self, logger::Logger},
};

use cli::Commands;

use crate::cli::Cli;
use crate::config::ClientConfig;

mod cli;
mod config;

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = cli::parse();
    let logger = Box::new(Logger::new(true, cli.verbose, false));
    logging::init(logger);

    let pwd = match std::env::current_dir() {
        Ok(mut path) => {
            path.push("config.json");
            path
        }
        Err(_) => PathBuf::new(),
    };

    let config_path = match &cli.config_path {
        Some(path) => path,
        None => {
            if pwd.is_file() {
                &pwd
            } else {
                &defaults::client_config_path()
            }
        }
    };

    let config = match config::ClientConfig::load(&config_path) {
        Ok(config) => Some(config),
        Err(_) => None,
    };

    let mut client = create_client(&config, &cli)?;

    let cmd = match &cli.command {
        Commands::Echo { message } => Command::Echo(message.to_string()),
        Commands::ListRepositories => Command::ListRepositories,
    };

    match client.send_command(&cmd) {
        Ok(response) => handle_response(&cmd, &response),
        Err(err) => panic!("{}", err),
    };
    Ok(())
}

fn create_client(config: &Option<ClientConfig>, cli: &Cli) -> io::Result<Client> {
    let socket_path = match &cli.socket {
        Some(path) => path,
        None => match config {
            Some(config) => &config.socket,
            None => &defaults::socket_path(),
        },
    };

    let tcp_addr = match &cli.host {
        Some(addr) => addr,
        None => &match config {
            Some(config) => format!("{}:{}", config.address, config.port),
            None => format!("{}:{}", defaults::localhost(), defaults::port()),
        },
    };

    let mut uds_client: bool = match config {
        Some(config) => !config.default_to_tcp,
        None => true,
    };

    if cli.uds {
        uds_client = true;
    }
    if cli.tcp {
        uds_client = false;
    }

    Ok(if uds_client {
        Client::new_uds(&socket_path)?
    } else {
        Client::new_tcp(&tcp_addr)?
    })
}

fn handle_response(command: &Command, response: &Response) {
    match response {
        Response::Success => info!("Successfully {command:#}"),
        Response::Message(message) => info!("{message}"),
        Response::Error(err) => error!("{err}"),
        Response::RepositoryList(list) => {
            for repo in list {
                info!("{repo}");
            }
        }
    };
}
