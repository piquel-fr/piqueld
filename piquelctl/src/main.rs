use std::io::{self};
use std::panic;

use piquelcore::ipc::client::{IpcClient, TcpClient, UdsClient};
use piquelcore::ipc::message::{Command, Response};

mod cli;
use cli::Commands;

fn main() -> io::Result<()> {
    let cli = cli::parse();

    let mut client: Box<dyn IpcClient> = match cli.host {
        Some(addr) => Box::new(TcpClient::new(&addr)?),
        None => Box::new(UdsClient::new(&cli.socket)?),
    };

    let cmd = match &cli.command {
        Commands::Hostname => Command::Status,
        Commands::Echo { message } => Command::Echo(message.to_string()),
    };

    match client.send_command(&cmd) {
        Ok(response) => {
            let resp_msg: &str = match response {
                Response::Ok => "Ok",
                Response::Message(message) => &format!("Message: \"{message}\""),
                Response::Error(err) => &format!("Error: \"{err}\""),
            };
            println!("[client] Received {resp_msg}");
        }
        Err(err) => panic!("{}", err),
    };

    println!("[client] Done. Closing connection.");
    Ok(())
}
