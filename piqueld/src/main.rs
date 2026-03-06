use std::io;

use piquelcore::ipc::server::Server;

#[tokio::main]
async fn main() -> io::Result<()> {
    Server::new().listen().await
}
