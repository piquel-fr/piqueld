use log::{debug, error, info};
use piquel::ipc::{
    ConnectionType,
    message::{Command, Response},
};
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener};

use crate::State;

pub struct Server {
    state: State,
    uds_path: PathBuf,
    address: String,
    port: u16,
}

type Result<T> = std::result::Result<T, ServerError>;

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("failed to connect to {conn_type} socket: {source}")]
    ConnectionError {
        conn_type: ConnectionType,
        #[source]
        source: std::io::Error,
    },
    #[error("I/O error in server: {0}")]
    IoError(#[from] std::io::Error),
    #[error("error during serialization of request of response: {0}")]
    SerializationError(#[from] serde_json::Error),
}

impl Server {
    pub fn new(state: State, (address, port): (String, u16), uds_path: PathBuf) -> Self {
        Server {
            state,
            uds_path,
            address,
            port,
        }
    }
    pub async fn listen(self) -> Result<()> {
        let server = Arc::new(self);
        tokio::try_join!(server.clone().listen_tcp(), server.clone().listen_uds())?;
        Ok(())
    }
    async fn listen_tcp(self: Arc<Self>) -> Result<()> {
        let conn_type = ConnectionType::Tcp;
        let addr = format!("{}:{}", self.address, self.port);
        let listener = TcpListener::bind(&addr)
            .await
            .map_err(|source| ServerError::ConnectionError { conn_type, source })?;
        info!("[{conn_type}] Listening on {addr}");
        loop {
            let (stream, _) = listener.accept().await?;
            let server = Arc::clone(&self);
            tokio::spawn(async move { server.handle(conn_type, stream).await });
        }
    }
    async fn listen_uds(self: Arc<Self>) -> Result<()> {
        if self.uds_path.exists() {
            std::fs::remove_file(&self.uds_path)?;
        }
        let conn_type = ConnectionType::Uds;
        let listener = UnixListener::bind(&self.uds_path)
            .map_err(|source| ServerError::ConnectionError { conn_type, source })?;
        info!("[{conn_type}] Listening on {:?}", self.uds_path);
        loop {
            let (stream, _) = listener.accept().await?;
            let server = Arc::clone(&self);
            tokio::spawn(async move { server.handle(conn_type, stream).await });
        }
    }
    async fn handle<T>(&self, conn_type: ConnectionType, mut stream: T) -> Result<()>
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
            let response = self.process_command(command).await.unwrap_or_else(|err| {
                error!("command failed: {err:#}");
                Response::Error(err.to_string())
            });
            let response_data = serde_json::to_vec(&response)?;
            let len = (response_data.len() as u32).to_be_bytes();
            stream.write_all(&len).await?;
            stream.write_all(&response_data).await?;
        }
    }
    async fn process_command(&self, command: Command) -> anyhow::Result<Response> {
        info!("Received Command: {command:#}");

        let git = &self.state.git;

        Ok(match command {
            Command::Echo(msg) => Response::Message(msg),
            // TODO: get the status
            Command::Status => Response::Message("Status OK".to_string()),
            Command::ListRepositories => {
                let repos = git
                    .list_repositories()
                    .await?
                    .iter()
                    .map(|repo| repo.full_name().to_string())
                    .collect();
                Response::RepositoryList(repos)
            }
            Command::DeleteRepository(full_name) => {
                let (owner, name) = match full_name.split_once("/") {
                    Some(tuple) => tuple,
                    None => {
                        return Ok(Response::Error(
                            "Repository name {full_name} is invalid".to_string(),
                        ));
                    }
                };
                match git.delete(owner.to_string(), name.to_string()).await {
                    Ok(_) => Response::Success,
                    Err(err) => Response::Error(err.to_string()),
                }
            }
        })
    }
}
