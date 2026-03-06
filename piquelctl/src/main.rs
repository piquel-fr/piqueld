use std::panic;

use piquelcore::config::{Config, defaults};
use piquelcore::ipc::client::Client;
use piquelcore::ipc::message::{Command, Response};

use cli::Commands;

mod cli;
mod config;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = cli::parse();

    let config_path = match cli.config_path {
        Some(path) => path,
        None => defaults::client_config_path(),
    };

    let socket_path = match cli.socket {
        Some(path) => path,
        None => match config::ClientConfig::load(&config_path) {
            Ok(config) => config.socket_path,
            Err(_) => defaults::socket_path(),
        },
    };

    let mut client: Client = match cli.host {
        Some(addr) => Client::new_tcp(&addr)?,
        None => Client::new_uds(&socket_path)?,
    };

    let cmd = match &cli.command {
        Commands::Hostname => Command::Hostname,
        Commands::Echo { message } => Command::Echo(message.to_string()),
    };

    match client.send_command(&cmd) {
        Ok(response) => handle_response(&cmd, &response),
        Err(err) => panic!("{}", err),
    };
    Ok(())
}

fn handle_response(command: &Command, response: &Response) {
    println!(
        "{}",
        match response {
            Response::Ok => "Success",
            Response::Message(message) => &message,
            Response::Error(err) => &err,
        }
    );
}
