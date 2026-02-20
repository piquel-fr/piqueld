use std::{io, net::TcpListener, os::unix::net::UnixListener};

use crate::ipc::ServerType;

pub trait Server {
    fn get_type(&self) -> ServerType;
    fn listen(&self) -> Option<io::Error>;
}

pub struct TcpServer {
    listener: TcpListener,
}

pub struct UdsServer {
    listener: UnixListener,
}
