use std::io;

use piquelcore::ipc::server::Server;

fn main() -> io::Result<()> {
    Server::new().listen()
}
