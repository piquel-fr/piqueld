use log::{debug, info};
use piquelcore::ipc::ConnectionType;
use piquelcore::ipc::message::{Command, Response};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener};

pub struct Server {
    uds_path: PathBuf,
    address: String,
    port: u16,
}

impl Server {
    pub fn new((address, port): (String, u16), uds_path: PathBuf) -> Self {
        Server {
            uds_path,
            address,
            port,
        }
    }
    pub async fn listen(self) -> Result<(), Box<dyn std::error::Error>> {
        let server = Arc::new(self);
        tokio::try_join!(server.clone().listen_tcp(), server.clone().listen_uds())?;
        Ok(())
    }
    async fn listen_tcp(self: Arc<Self>) -> Result<(), Box<dyn std::error::Error>> {
        let conn_type = ConnectionType::Tcp;
        let addr = format!("{}:{}", self.address, self.port);
        let listener = match TcpListener::bind(&addr).await {
            Ok(stream) => stream,
            Err(err) => return Err(format!("Failed to connect to TCP socket: {err:#}").into()),
        };
        info!("[{conn_type}] Listening on {addr}");
        loop {
            let (stream, _) = listener.accept().await?;
            let server = Arc::clone(&self);
            tokio::spawn(async move { server.handle(conn_type, stream).await });
        }
    }
    async fn listen_uds(self: Arc<Self>) -> Result<(), Box<dyn std::error::Error>> {
        if self.uds_path.exists() {
            std::fs::remove_file(&self.uds_path)?;
        }
        let conn_type = ConnectionType::Uds;
        let listener = match UnixListener::bind(&self.uds_path) {
            Ok(stream) => stream,
            Err(err) => return Err(format!("Failed to connect to Unix socket: {err:#}").into()),
        };
        info!("[{conn_type}] Listening on {:?}", self.uds_path);
        loop {
            let (stream, _) = listener.accept().await?;
            let server = Arc::clone(&self);
            tokio::spawn(async move { server.handle(conn_type, stream).await });
        }
    }
    async fn handle<T>(&self, conn_type: ConnectionType, mut stream: T) -> tokio::io::Result<()>
    where
        T: AsyncRead + AsyncWrite + Unpin,
    {
        debug!("[{conn_type}] Received connection");
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
        info!("Received Command: {command:#}");

        Ok(match command {
            Command::Echo(msg) => Response::Message(msg),
            Command::Status => Response::Message("Status OK".to_string()),
            Command::ListRepositories => {
                Response::RepositoryList(vec!["test".into(), "test2".into()])
            }
        })
    }
}
