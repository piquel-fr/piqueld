use std::{io, net::TcpListener, os::unix::net::UnixListener, path::Path};

use piquelcore::config::{LISTEN_ADDR, SOCKET_PATH};

pub struct Server {
    uds_listener: UnixListener,
    tcp_listener: TcpListener,
}

impl Server {
    pub fn new() -> io::Result<Self> {
        // Remove a leftover socket file from a previous run, if any.
        if Path::new(SOCKET_PATH).exists() {
            std::fs::remove_file(SOCKET_PATH)?;
        }

        let server = Server {
            uds_listener: UnixListener::bind(SOCKET_PATH)?,
            tcp_listener: TcpListener::bind(LISTEN_ADDR)?,
        };

        println!("[server] Listening on {SOCKET_PATH} and {LISTEN_ADDR}");

        Ok(server)
    }
}
