use crate::ipc::{
    ConnectionType,
    message::{Command, Response},
};

use std::{
    io::{self, Read, Write},
    net::TcpStream,
    os::unix::net::UnixStream,
    path::Path,
};

trait ReadWrite: Write + Read {}
impl<T: Write + Read> ReadWrite for T {}

pub struct Client {
    stream: Box<dyn ReadWrite>,
    client_type: ConnectionType,
}

impl Client {
    pub fn new_tcp(addr: &str) -> io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        Ok(Self {
            stream: Box::new(stream),
            client_type: ConnectionType::Tcp,
        })
    }
    pub fn new_uds(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        Ok(Self {
            stream: Box::new(stream),
            client_type: ConnectionType::Uds,
        })
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
    pub fn get_type(&self) -> ConnectionType {
        self.client_type
    }
}
