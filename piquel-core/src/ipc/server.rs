use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::thread;

use crate::config::{LISTEN_ADDR, SOCKET_PATH};
use crate::ipc::message::{Command, Response};

pub struct Server {
    uds_path: &'static str,
    tcp_addr: &'static str,
}

impl Server {
    pub fn new() -> Self {
        Server {
            uds_path: SOCKET_PATH,
            tcp_addr: LISTEN_ADDR,
        }
    }
    pub fn listen(&self) -> io::Result<()> {
        let tcp_handle = thread::spawn(|| listen_tcp(self.tcp_addr));
        let uds_handle = thread::spawn(|| listen_uds(self.uds_path));

        let _ = tcp_handle.join();
        let _ = uds_handle.join();
        Ok(())
    }
}

fn listen_tcp(addr: &str) -> io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    println!("[TCP] Listening on {addr}");

    loop {
        let (stream, _) = listener.accept()?;
        thread::spawn(move || handle(stream));
    }
}

fn listen_uds(path: &str) -> io::Result<()> {
    if Path::new(path).exists() {
        std::fs::remove_file(path)?;
    }

    let listener = UnixListener::bind(path)?;
    println!("[UDS] Listening on {path}");

    loop {
        let (stream, _) = listener.accept()?;
        thread::spawn(move || handle(stream));
    }
}

fn handle<T>(mut stream: T) -> io::Result<()>
where
    T: Read + Write,
{
    loop {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf)?;

        let len = u32::from_be_bytes(len_buf) as usize;

        let mut cmd_buf = vec![0u8; len];
        stream.read_exact(&mut cmd_buf)?;
        let command: Command = serde_json::from_slice(&cmd_buf)?;

        let response = process_command(command)?;

        let response_data = serde_json::to_vec(&response)?;
        let len = (response_data.len() as u32).to_be_bytes();
        stream.write_all(&len)?;
        stream.write_all(&response_data)?;
    }
}

// TODO: processing should be handled by server
fn process_command(command: Command) -> io::Result<Response> {
    Ok(match command {
        Command::Status => Response::Message("Status OK".to_string()),
        Command::Hostname => {
            // TODO: hostname
            Response::Message("waiting for std::net::hostname() to become available".to_string())
        }
        Command::Echo(msg) => Response::Message(msg),
        Command::Reload => {
            println!("Received reload command");
            Response::Ok
        }
        Command::Stop => {
            println!("Received stop command");
            Response::Ok
        }
    })
}
