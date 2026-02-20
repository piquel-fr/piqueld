use std::io::{self};
use std::panic;

use piquelcore::config::SOCKET_PATH;
use piquelcore::ipc::client::{Client, UdsClient};
use piquelcore::ipc::message::{Command, Response};

fn main() -> io::Result<()> {
    let mut client = match UdsClient::new() {
        Ok(client) => client,
        Err(err) => panic!("{}", err),
    };

    println!("[client] Connected to {SOCKET_PATH}");

    let messages = ["Hello, server!", "How's IPC treating you?", "Goodbye!"];

    for msg in &messages {
        println!("[client] Sending: \"{msg}\"");
        match client.send_command(&Command::Echo(msg.to_string())) {
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
