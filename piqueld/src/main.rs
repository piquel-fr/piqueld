use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

const ADDR: &str = "127.0.0.1:7878";

fn handle_client(mut stream: TcpStream) {
    let peer = stream.peer_addr().unwrap();
    println!("[server] Connection from {peer}");

    loop {
        // --- Read the 4-byte length prefix ---
        let mut len_buf = [0u8; 4];
        match stream.read_exact(&mut len_buf) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                println!("[server] Client {peer} disconnected.");
                return;
            }
            Err(e) => {
                eprintln!("[server] Error reading length from {peer}: {e}");
                return;
            }
        }

        let msg_len = u32::from_be_bytes(len_buf) as usize;

        // --- Read exactly `msg_len` bytes ---
        let mut msg_buf = vec![0u8; msg_len];
        if let Err(e) = stream.read_exact(&mut msg_buf) {
            eprintln!("[server] Error reading message body from {peer}: {e}");
            return;
        }

        let message = String::from_utf8_lossy(&msg_buf);
        println!("[server] Received from {peer}: \"{message}\"");

        // --- Echo the message back with the same framing ---
        let response = format!("Echo: {message}");
        let response_bytes = response.as_bytes();
        let response_len = (response_bytes.len() as u32).to_be_bytes();

        if let Err(e) = stream.write_all(&response_len) {
            eprintln!("[server] Error writing response length to {peer}: {e}");
            return;
        }
        if let Err(e) = stream.write_all(response_bytes) {
            eprintln!("[server] Error writing response body to {peer}: {e}");
            return;
        }
    }
}

fn main() -> io::Result<()> {
    let listener = TcpListener::bind(ADDR)?;
    println!("[server] Listening on {ADDR}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                // Spawn a new thread for each incoming connection.
                thread::spawn(|| handle_client(stream));
            }
            Err(e) => {
                eprintln!("[server] Failed to accept connection: {e}");
            }
        }
    }

    Ok(())
}
