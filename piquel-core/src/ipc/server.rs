use crate::ipc::message::{Command, Response};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener};

pub struct Server {
    uds_path: PathBuf,
    tcp_addr: String,
}

impl Server {
    pub fn new(tcp_addr: String, uds_path: PathBuf) -> Self {
        Server { uds_path, tcp_addr }
    }
    pub async fn listen(self) -> tokio::io::Result<()> {
        let server = Arc::new(self);
        tokio::try_join!(server.clone().listen_tcp(), server.clone().listen_uds(),)?;
        Ok(())
    }
    async fn listen_tcp(self: Arc<Self>) -> tokio::io::Result<()> {
        let listener = TcpListener::bind(&self.tcp_addr).await?;
        println!("[TCP] Listening on {}", self.tcp_addr);
        loop {
            let (stream, _) = listener.accept().await?;
            let server = Arc::clone(&self);
            tokio::spawn(async move { server.handle(stream).await });
        }
    }
    async fn listen_uds(self: Arc<Self>) -> tokio::io::Result<()> {
        if self.uds_path.exists() {
            std::fs::remove_file(&self.uds_path)?;
        }
        let listener = UnixListener::bind(&self.uds_path)?;
        println!("[UDS] Listening on {:?}", self.uds_path);
        loop {
            let (stream, _) = listener.accept().await?;
            let server = Arc::clone(&self);
            tokio::spawn(async move { server.handle(stream).await });
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
    fn process_command(&self, command: Command) -> tokio::io::Result<Response> {
        Ok(match command {
            Command::Status => Response::Message("Status OK".to_string()),
            Command::Hostname => Response::Message(
                "waiting for std::net::hostname() to become available".to_string(),
            ),
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
