use std::io::{self};
use std::panic;

use piquelcore::ipc::client::Client;
use piquelcore::ipc::message::{Command, Response};

mod cli;
use cli::Commands;

fn main() -> io::Result<()> {
    let cli = cli::parse();

    let mut client: Client = match cli.host {
        Some(addr) => Client::new_tcp(&addr)?,
        None => Client::new_uds(&cli.socket)?,
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
