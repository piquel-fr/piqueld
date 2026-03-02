use piquelcore::ipc::ConnectionType;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub struct Connection<T: AsyncRead + AsyncWrite + Unpin> {
    connection_type: ConnectionType,
    stream: T,
}

impl<T: AsyncRead + AsyncWrite + Unpin> Connection<T> {
    pub fn get_type(&self) -> ConnectionType {
        self.connection_type
    }
    pub async fn handle(connection_type: ConnectionType, mut stream: T) -> tokio::io::Result<()> {
        let label = match connection_type {
            ConnectionType::Tcp => "TCP",
            ConnectionType::Uds => "UDS",
        };

        loop {
            let mut len_buf = [0u8; 4];
            match stream.read(&mut len_buf).await {
                Ok(0) => {
                    println!("[{label}] Disconnected");
                    return Ok(());
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("[{label}] Error: {e}");
                    break;
                }
            }

            let msg_len = u32::from_be_bytes(len_buf) as usize;

            // --- Read exactly `msg_len` bytes ---
            let mut msg_buf = vec![0u8; msg_len];
            if let Err(e) = stream.read(&mut msg_buf).await {
                eprintln!("[server] Error reading message body: {e}");
                return Ok(());
            }

            let message = String::from_utf8_lossy(&msg_buf);
            println!("[server] Received: \"{message}\"");

            // --- Echo the message back with the same framing ---
            let response = format!("Echo: {message}");
            let response_bytes = response.as_bytes();
            let response_len = (response_bytes.len() as u32).to_be_bytes();

            if let Err(e) = stream.write(&response_len).await {
                eprintln!("[server] Error writing response length: {e}");
                return Ok(());
            }
            if let Err(e) = stream.write(response_bytes).await {
                eprintln!("[server] Error writing response body: {e}");
                return Ok(());
            }
        }

        Ok(())
    }
}
