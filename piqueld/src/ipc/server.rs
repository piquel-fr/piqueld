use std::path::Path;

use piquelcore::{
    config::{LISTEN_ADDR, SOCKET_PATH},
    ipc::ConnectionType,
};
use tokio::net::{TcpListener, UnixListener};

use crate::ipc::connection::Connection;

pub struct Server<'a> {
    uds_path: &'a str,
    tcp_addr: &'a str,
}

impl<'a> Server<'a> {
    pub fn new() -> Self {
        Server {
            uds_path: SOCKET_PATH,
            tcp_addr: LISTEN_ADDR,
        }
    }
    pub async fn listen(&self) -> tokio::io::Result<()> {
        tokio::try_join!(self.listen_tcp(), self.listen_uds());
        Ok(())
    }
    async fn listen_tcp(&self) -> tokio::io::Result<()> {
        let listener = TcpListener::bind(self.tcp_addr).await?;
        println!("[TCP] Listening on {LISTEN_ADDR}");

        loop {
            let (stream, _) = listener.accept().await?;
            tokio::spawn(Connection::handle(ConnectionType::Tcp, stream));
        }
    }
    async fn listen_uds(&self) -> tokio::io::Result<()> {
        // Remove a leftover socket file from a previous run, if any.
        if Path::new(self.uds_path).exists() {
            std::fs::remove_file(self.uds_path)?;
        }

        let listener = UnixListener::bind(self.uds_path)?;
        println!("[UDS] Listening on {SOCKET_PATH}");

        loop {
            let (stream, _) = listener.accept().await?;
            tokio::spawn(Connection::handle(ConnectionType::Uds, stream));
        }
    }
}
