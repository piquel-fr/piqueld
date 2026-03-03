use std::io::{self};
use std::panic;

use piquelcore::ipc::client::{Client, TcpClient, UdsClient};
use piquelcore::ipc::message::{Command, Response};

mod cli;

fn main() -> io::Result<()> {
    let cli = cli::parse();

    let client;
    if let Some(host) = cli.host {
        client = TcpClient::new()?;
    } else {
        client = UdsClient::new()?;
    }

    let commands = [
        Command::Status,
        Command::Echo("Hello, server!".to_string()),
        Command::Echo("How's IPC treating you?".to_string()),
        Command::Echo("Goodbye!".to_string()),
        Command::Stop,
    ];

    for cmd in commands {
        match client.send_command(&cmd) {
            Ok(response) => {
                let resp_msg: &str = match response {
                    Response::Ok => "Ok",
                    Response::Message(message) => &format!("Message: \"{message}\""),
                    Response::Error(err) => &format!("Error: \"{err}\""),
                };
                println!("[client] Received: \"{resp_msg}\"");
            }
            Err(err) => panic!("{}", err),
        };
    }

    println!("[client] Done. Closing connection.");
    Ok(())
}
