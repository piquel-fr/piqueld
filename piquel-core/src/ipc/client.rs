use crate::{
    config::{LISTEN_ADDR, SOCKET_PATH},
    ipc::ConnectionType,
};

use super::message::{Command, Response};
use std::{
    io::{self, Read, Write},
    net::TcpStream,
    os::unix::net::UnixStream,
    usize,
};

pub struct Client<T: Read + Write> {
    stream: T,
    client_type: ConnectionType,
}

impl<T: Read + Write> Client<T> {
    pub fn get_type(&self) -> ConnectionType {
        self.client_type
    }
    pub fn send_command(&mut self, command: &Command) -> io::Result<Response> {
        let request = serde_json::to_vec(&command)?;
        let len = (request.len() as u32).to_be_bytes();
        self.stream.write_all(&len)?;
        self.stream.write_all(&request)?;

        let mut len_buf = [0u8; 4];
        self.stream.read_exact(&mut len_buf)?;
        let len = u32::from_be_bytes(len_buf) as usize;

        let mut response_buf = vec![0u8; len];
        self.stream.read_exact(&mut response_buf)?;

        let response: Response = serde_json::from_slice(&response_buf)?;
        Ok(response)
    }
}

pub type TcpClient = Client<TcpStream>;

impl TcpClient {
    pub fn new() -> io::Result<Self> {
        let stream = TcpStream::connect(LISTEN_ADDR)?;
        println!("[client] Connected to {LISTEN_ADDR}");
        Ok(Self {
            stream,
            client_type: ConnectionType::Tcp,
        })
    }
}

pub type UdsClient = Client<UnixStream>;

impl UdsClient {
    pub fn new() -> io::Result<Self> {
        let stream = UnixStream::connect(SOCKET_PATH)?;
        println!("[client] Connected to {SOCKET_PATH}");
        Ok(Self {
            stream,
            client_type: ConnectionType::Uds,
        })
    }
}
