use piquelcore::ipc::ConnectionType;
use tokio::io::{AsyncRead, AsyncWrite};

pub struct Connection<T: AsyncRead + AsyncWrite> {
    connection_type: ConnectionType,
    stream: T,
}

impl<T: AsyncRead + AsyncWrite> Connection<T> {
    pub fn get_type(&self) -> ConnectionType {
        self.connection_type
    }
    pub async fn handle(connection_type: ConnectionType, stream: T) -> tokio::io::Result<()> {
        let connection = Connection {
            connection_type,
            stream,
        };

        Ok(())
    }
}
