use crate::config::{LISTEN_ADDR, SOCKET_PATH};

use super::message::{Command, Response};
use std::{error::Error, io, net::TcpStream, os::unix::net::UnixStream};

pub enum ClientType {
    TcpClient,
    UdsClient,
}

pub trait Client {
    fn run_command(&self, command: &Command) -> Result<Response, Box<dyn Error>>;
    fn get_type(&self) -> ClientType;
}

pub struct TcpClient {
    pub stream: TcpStream,
}

impl TcpClient {
    pub fn new() -> Result<TcpClient, bool> {
        let stream = match TcpStream::connect(LISTEN_ADDR) {
            Ok(tcp_stream) => tcp_stream,
            Err(_) => return Err(false),
        };

        Ok(TcpClient { stream })
    }
}

impl Client for TcpClient {
    fn get_type(&self) -> ClientType {
        ClientType::TcpClient
    }
    fn run_command(&self, command: &Command) -> Result<Response, Box<dyn Error>> {
        Result::Ok(Response::Ok)
    }
}

pub struct UdsClient {
    pub stream: UnixStream,
}

impl UdsClient {
    pub fn new() -> Result<UdsClient, io::Error> {
        let stream = match UnixStream::connect(SOCKET_PATH) {
            Ok(tcp_stream) => tcp_stream,
            Err(err) => return Err(err),
        };

        Ok(UdsClient { stream })
    }
}

impl Client for UdsClient {
    fn get_type(&self) -> ClientType {
        ClientType::UdsClient
    }
    fn run_command(&self, command: &Command) -> Result<Response, Box<dyn Error>> {
        Result::Ok(Response::Ok)
    }
}
