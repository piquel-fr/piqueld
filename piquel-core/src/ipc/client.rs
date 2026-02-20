use crate::config::{LISTEN_ADDR, SOCKET_PATH};

use super::message::{Command, Response};
use std::{
    io::{self, Read, Write},
    net::TcpStream,
    os::unix::net::UnixStream,
    usize,
};

pub enum ClientType {
    TcpClient,
    UdsClient,
}

pub trait Client {
    fn send_command(&mut self, command: &Command) -> io::Result<Response>;
    fn get_type(&self) -> ClientType;
}

pub struct TcpClient {
    pub stream: TcpStream,
}

impl TcpClient {
    pub fn new() -> io::Result<TcpClient> {
        let stream = TcpStream::connect(LISTEN_ADDR)?;
        Ok(TcpClient { stream })
    }
}

impl Client for TcpClient {
    fn get_type(&self) -> ClientType {
        ClientType::TcpClient
    }
    fn send_command(&mut self, command: &Command) -> io::Result<Response> {
        let message = serialize_command(command)?;
        write_message(&mut self.stream, &message)?;
        let response = read_message(&mut self.stream)?;
        Ok(Response::Message(response))
    }
}

pub struct UdsClient {
    pub stream: UnixStream,
}

impl UdsClient {
    pub fn new() -> io::Result<UdsClient> {
        let stream = UnixStream::connect(SOCKET_PATH)?;
        Ok(UdsClient { stream })
    }
}

impl Client for UdsClient {
    fn get_type(&self) -> ClientType {
        ClientType::UdsClient
    }
    fn send_command(&mut self, command: &Command) -> io::Result<Response> {
        let message = serialize_command(command)?;
        write_message(&mut self.stream, &message)?;
        let response = read_message(&mut self.stream)?;
        Ok(Response::Message(response))
    }
}

fn write_message<T: Write>(stream: &mut T, message: &str) -> io::Result<usize> {
    let bytes = message.as_bytes();
    let len = bytes.len() as usize;

    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(bytes)?;

    Ok(len)
}

fn read_message<T: Read>(stream: &mut T) -> io::Result<String> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let msg_len = u32::from_be_bytes(len_buf) as usize;

    let mut msg_buf = vec![0u8; msg_len];
    stream.read_exact(&mut msg_buf)?;

    Ok(String::from_utf8_lossy(&msg_buf).into_owned())
}

fn serialize_command(command: &Command) -> io::Result<String> {
    Ok(match command {
        Command::Echo(msg) => msg.to_string(),
        _ => "No Content".to_string(),
    })
}
