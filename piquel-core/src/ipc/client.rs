use crate::ipc::ConnectionType;

use super::message::{Command, Response};
use std::{
    io::{self, Read, Write},
    net::TcpStream,
    os::unix::net::UnixStream,
    usize,
};

pub trait IpcClient {
    fn send_command(&mut self, command: &Command) -> io::Result<Response>;
    fn get_type(&self) -> ConnectionType;
}

impl<T: Read + Write> IpcClient for Client<T> {
    fn send_command(&mut self, command: &Command) -> io::Result<Response> {
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
    fn get_type(&self) -> ConnectionType {
        self.client_type
    }
}

pub struct Client<T: Read + Write> {
    stream: T,
    client_type: ConnectionType,
}

pub type TcpClient = Client<TcpStream>;

impl TcpClient {
    pub fn new(addr: &str) -> io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        println!("[client] Connected to {addr}");
        Ok(Self {
            stream,
            client_type: ConnectionType::Tcp,
        })
    }
}

pub type UdsClient = Client<UnixStream>;

impl UdsClient {
    pub fn new(path: &str) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        println!("[client] Connected to {path}");
        Ok(Self {
            stream,
            client_type: ConnectionType::Uds,
        })
    }
}
