use std::{io, net::TcpListener, os::unix::net::UnixListener};

use crate::ipc::ConnectionType;

pub trait Server {
    fn get_type(&self) -> ConnectionType;
    fn listen(&self) -> Option<io::Error>;
}

pub struct TcpServer {
    listener: TcpListener,
}

pub struct UdsServer {
    listener: UnixListener,
}
