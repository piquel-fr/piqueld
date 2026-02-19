use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;

use piquelcore::config::SOCKET_PATH;

/// Sends a length-prefixed message over the stream.
fn send_message(stream: &mut UnixStream, message: &str) -> io::Result<()> {
    let bytes = message.as_bytes();
    let len = (bytes.len() as u32).to_be_bytes();

    stream.write_all(&len)?;
    stream.write_all(bytes)?;
    Ok(())
}

/// Reads a length-prefixed message from the stream.
fn recv_message(stream: &mut UnixStream) -> io::Result<String> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let msg_len = u32::from_be_bytes(len_buf) as usize;

    let mut msg_buf = vec![0u8; msg_len];
    stream.read_exact(&mut msg_buf)?;

    Ok(String::from_utf8_lossy(&msg_buf).into_owned())
}

fn main() -> io::Result<()> {
    let mut stream = UnixStream::connect(SOCKET_PATH)?;
    println!("[client] Connected to {SOCKET_PATH}");

    let messages = ["Hello, server!", "How's IPC treating you?", "Goodbye!"];

    for msg in &messages {
        println!("[client] Sending: \"{msg}\"");
        send_message(&mut stream, msg)?;

        let response = recv_message(&mut stream)?;
        println!("[client] Received: \"{response}\"");
    }

    println!("[client] Done. Closing connection.");
    Ok(())
}
