use crate::ipc::server::Server;

mod ipc;

#[tokio::main]
async fn main() -> tokio::io::Result<()> {
    let server = Server::new();
    server.listen().await?;
    Ok(())
}
