use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::thread;

use piquelcore::config::SOCKET_PATH;

fn handle_client(mut stream: UnixStream) {
    // UnixStream doesn't have a peer_addr in the same way, so we use a placeholder.
    println!("[server] New client connected.");

    loop {
        // --- Read the 4-byte length prefix ---
        let mut len_buf = [0u8; 4];
        match stream.read_exact(&mut len_buf) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                println!("[server] Client disconnected.");
                return;
            }
            Err(e) => {
                eprintln!("[server] Error reading length: {e}");
                return;
            }
        }

        let msg_len = u32::from_be_bytes(len_buf) as usize;

        // --- Read exactly `msg_len` bytes ---
        let mut msg_buf = vec![0u8; msg_len];
        if let Err(e) = stream.read_exact(&mut msg_buf) {
            eprintln!("[server] Error reading message body: {e}");
            return;
        }

        let message = String::from_utf8_lossy(&msg_buf);
        println!("[server] Received: \"{message}\"");

        // --- Echo the message back with the same framing ---
        let response = format!("Echo: {message}");
        let response_bytes = response.as_bytes();
        let response_len = (response_bytes.len() as u32).to_be_bytes();

        if let Err(e) = stream.write_all(&response_len) {
            eprintln!("[server] Error writing response length: {e}");
            return;
        }
        if let Err(e) = stream.write_all(response_bytes) {
            eprintln!("[server] Error writing response body: {e}");
            return;
        }
    }
}

fn main() -> io::Result<()> {
    // Remove a leftover socket file from a previous run, if any.
    if Path::new(SOCKET_PATH).exists() {
        std::fs::remove_file(SOCKET_PATH)?;
    }

    let listener = UnixListener::bind(SOCKET_PATH)?;
    println!("[server] Listening on {SOCKET_PATH}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                thread::spawn(|| handle_client(stream));
            }
            Err(e) => {
                eprintln!("[server] Failed to accept connection: {e}");
            }
        }
    }

    Ok(())
}
