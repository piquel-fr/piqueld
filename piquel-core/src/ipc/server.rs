use std::path::Path;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener};

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
    pub async fn listen(&self) -> tokio::io::Result<()> {
        tokio::try_join!(
            self.listen_tcp(self.tcp_addr),
            self.listen_uds(self.uds_path)
        )?;
        Ok(())
    }
    async fn listen_tcp(&self, addr: &str) -> tokio::io::Result<()> {
        let listener = TcpListener::bind(addr).await?;
        println!("[TCP] Listening on {addr}");

        loop {
            let (stream, _) = listener.accept().await?;
            tokio::spawn(self.handle(stream));
        }
    }

    async fn listen_uds(&self, path: &str) -> tokio::io::Result<()> {
        if Path::new(path).exists() {
            std::fs::remove_file(path)?;
        }

        let listener = UnixListener::bind(path)?;
        println!("[UDS] Listening on {path}");

        loop {
            let (stream, _) = listener.accept().await?;
            tokio::spawn(self.handle(stream));
        }
    }

    async fn handle<T>(&self, mut stream: T) -> tokio::io::Result<()>
    where
        T: AsyncRead + AsyncWrite + Unpin,
    {
        loop {
            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf).await?;

            let len = u32::from_be_bytes(len_buf) as usize;

            let mut cmd_buf = vec![0u8; len];
            stream.read_exact(&mut cmd_buf).await?;
            let command: Command = serde_json::from_slice(&cmd_buf)?;

            let response = self.process_command(command)?;

            let response_data = serde_json::to_vec(&response)?;
            let len = (response_data.len() as u32).to_be_bytes();
            stream.write_all(&len).await?;
            stream.write_all(&response_data).await?;
        }
    }

    // TODO: processing should be handled by server
    fn process_command(&self, command: Command) -> tokio::io::Result<Response> {
        Ok(match command {
            Command::Status => Response::Message("Status OK".to_string()),
            Command::Hostname => {
                // TODO: hostname
                Response::Message(
                    "waiting for std::net::hostname() to become available".to_string(),
                )
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
}
