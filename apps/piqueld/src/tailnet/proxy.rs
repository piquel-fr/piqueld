//! Loopback listener for connections forwarded by tailscaled. Each connection
//! starts with a PROXY protocol v2 header carrying the client's tailnet address.
//!
//! Any local process can connect to a loopback port and write such a header,
//! so connections are accepted only from processes running as the daemon's
//! own user, like the `tailscaled` it starts. Those could already read the
//! daemon's database; anyone else could otherwise pose as any tailnet peer.

use axum::serve::Listener;
use std::{
    io::{Error, ErrorKind},
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

/// tailscaled writes the header as soon as it connects.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
/// Connections queued between header parsing and the HTTP server.
const QUEUE: usize = 64;
const SIGNATURE: [u8; 12] = *b"\r\n\r\n\0\r\nQUIT\n";
/// Version 2 with the PROXY command; `LOCAL` health checks carry no client.
const PROXY_COMMAND: u8 = 0x21;
const TCP_V4: u8 = 0x11;
const TCP_V6: u8 = 0x21;

/// Forwarded connections paired with their tailnet client addresses.
pub struct ProxyListener {
    connections: mpsc::Receiver<(TcpStream, SocketAddr)>,
    address: SocketAddr,
}

impl ProxyListener {
    /// Reads headers from connections on `listener`, bound to `address`, until
    /// cancellation.
    /// Connections with a missing or malformed header are dropped.
    pub fn spawn(
        listener: TcpListener,
        address: SocketAddr,
        cancellation: CancellationToken,
    ) -> Self {
        let (sender, connections) = mpsc::channel(QUEUE);
        tokio::spawn(Self::accept(listener, address, sender, cancellation));
        Self {
            connections,
            address,
        }
    }

    async fn accept(
        mut listener: TcpListener,
        address: SocketAddr,
        connections: mpsc::Sender<(TcpStream, SocketAddr)>,
        cancellation: CancellationToken,
    ) {
        let daemon = rustix::process::geteuid().as_raw();
        loop {
            let (mut stream, local) = tokio::select! {
                () = cancellation.cancelled() => return,
                accepted = Listener::accept(&mut listener) => accepted,
            };
            let connections = connections.clone();
            // Headers are read concurrently so one stalled connection cannot block others.
            tokio::spawn(async move {
                let owner = owner(local, address).await;
                if owner != Some(daemon) {
                    tracing::warn!(%local, ?owner, "refusing a forwarded tailnet connection from another user");
                    return;
                }
                let header = tokio::time::timeout(HEADER_TIMEOUT, Self::client(&mut stream))
                    .await
                    .map_err(Error::from);
                match header.and_then(|client| client) {
                    Ok(client) => {
                        // A closed queue means the server is shutting down.
                        let _ = connections.send((stream, client)).await;
                    }
                    Err(error) => {
                        tracing::debug!(%local, %error, "dropping forwarded tailnet connection");
                    }
                }
            });
        }
    }

    /// Reads a PROXY v2 header and returns the client address it carries.
    async fn client(stream: &mut (impl AsyncRead + Unpin)) -> std::io::Result<SocketAddr> {
        let invalid = |message| Error::new(ErrorKind::InvalidData, message);
        let mut header = [0; 16];
        stream.read_exact(&mut header).await?;
        if header[..12] != SIGNATURE || header[12] != PROXY_COMMAND {
            return Err(invalid("missing PROXY v2 header"));
        }
        let mut addresses = vec![0; usize::from(u16::from_be_bytes([header[14], header[15]]))];
        stream.read_exact(&mut addresses).await?;
        match header[13] {
            TCP_V4 => Self::source::<4>(&addresses),
            TCP_V6 => Self::source::<16>(&addresses),
            _ => None,
        }
        .ok_or_else(|| invalid("PROXY header has no TCP over IPv4 or IPv6 addresses"))
    }

    /// Extracts the source from an address block laid out as source address,
    /// destination address, source port, destination port.
    fn source<const N: usize>(addresses: &[u8]) -> Option<SocketAddr>
    where
        IpAddr: From<[u8; N]>,
    {
        let (source, rest) = addresses.split_first_chunk::<N>()?;
        let port = rest.get(N..)?.first_chunk::<2>()?;
        Some(SocketAddr::new(
            IpAddr::from(*source),
            u16::from_be_bytes(*port),
        ))
    }
}

/// The user owning the local end of the loopback TCP connection from `peer`
/// to `listener`, read from `/proc/net/tcp` or `/proc/net/tcp6`.
async fn owner(peer: SocketAddr, listener: SocketAddr) -> Option<u32> {
    let table = if peer.is_ipv4() {
        "/proc/net/tcp"
    } else {
        "/proc/net/tcp6"
    };
    let table = tokio::fs::read_to_string(table).await.ok()?;
    owner_in(&table, peer, listener)
}

/// Finds the open socket bound to `local` and connected to `remote` in a
/// `/proc/net/tcp`-style table and returns its owner's user ID.
///
/// Only established sockets still held by a process (nonzero inode) count:
/// once a client closes, the kernel keeps its row with owner 0, which would
/// otherwise vouch for whatever the client queued before closing.
fn owner_in(table: &str, local: SocketAddr, remote: SocketAddr) -> Option<u32> {
    const ESTABLISHED: &str = "01";
    let (local, remote) = (proc_address(local), proc_address(remote));
    table.lines().skip(1).find_map(|line| {
        let columns: Vec<&str> = line.split_whitespace().collect();
        let open = columns.get(1) == Some(&local.as_str())
            && columns.get(2) == Some(&remote.as_str())
            && columns.get(3) == Some(&ESTABLISHED)
            && columns.get(9).is_some_and(|inode| *inode != "0");
        open.then(|| columns.get(7)?.parse().ok()).flatten()
    })
}

/// An address as the kernel prints it in `/proc/net/tcp`: each 32-bit word of
/// the address in native byte order, then the port, all in uppercase hex.
fn proc_address(address: SocketAddr) -> String {
    use std::fmt::Write as _;
    let octets = match address.ip() {
        IpAddr::V4(ip) => ip.octets().to_vec(),
        IpAddr::V6(ip) => ip.octets().to_vec(),
    };
    let mut text = String::new();
    // Writing to a `String` cannot fail.
    for word in octets.as_chunks::<4>().0 {
        let _ = write!(text, "{:08X}", u32::from_ne_bytes(*word));
    }
    let _ = write!(text, ":{:04X}", address.port());
    text
}

impl Listener for ProxyListener {
    type Io = TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.connections.recv().await {
            Some(connection) => connection,
            // The accept task only stops on shutdown, which also stops serving.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.address)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    fn header(family: u8, addresses: &[u8]) -> Vec<u8> {
        let mut header = SIGNATURE.to_vec();
        header.extend([PROXY_COMMAND, family]);
        header.extend(u16::try_from(addresses.len()).unwrap().to_be_bytes());
        header.extend(addresses);
        header
    }

    #[tokio::test]
    async fn reads_ipv4_and_ipv6_clients_and_leaves_the_payload() {
        let v4 = [[100, 64, 0, 1], [100, 64, 0, 2]].concat();
        let mut v4 = header(TCP_V4, &[v4, vec![0x9c, 0x40, 0x01, 0xbb]].concat());
        v4.extend(b"GET /");
        let mut stream = v4.as_slice();
        assert_eq!(
            ProxyListener::client(&mut stream).await.unwrap(),
            "100.64.0.1:40000".parse().unwrap()
        );
        assert_eq!(stream, b"GET /");

        let client: Ipv6Addr = "fd7a:115c:a1e0::1".parse().unwrap();
        let v6 = [
            client.octets().as_slice(),
            &Ipv6Addr::LOCALHOST.octets(),
            &[0x9c, 0x40, 0x01, 0xbb],
            // Trailing TLVs are skipped.
            &[0x04, 0x00, 0x01, 0x00],
        ]
        .concat();
        assert_eq!(
            ProxyListener::client(&mut header(TCP_V6, &v6).as_slice())
                .await
                .unwrap(),
            SocketAddr::new(client.into(), 40000)
        );
    }

    /// The owner of a forwarded connection is read from the socket table, so
    /// a connection from another user's process is told apart and refused, as
    /// is one whose client has already closed.
    #[test]
    fn connections_are_attributed_to_their_owner() {
        let peer: SocketAddr = "127.0.0.1:40000".parse().unwrap();
        let listener: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        // Columns: local, remote, state, queues, timer, retransmits, uid, timeout, inode.
        let line = |local: SocketAddr, remote: SocketAddr, state: &str, uid: u32, inode: u32| {
            let (local, remote) = (proc_address(local), proc_address(remote));
            format!("   0: {local} {remote} {state} 0:0 0:0 0 {uid} 0 {inode} 1")
        };
        let table = |rows: [String; 2]| format!("  sl  header\n{}\n{}", rows[0], rows[1]);
        let open = table([
            line(listener, peer, "01", 1000, 7),
            line(peer, listener, "01", 1001, 8),
        ]);
        assert_eq!(owner_in(&open, peer, listener), Some(1001));
        let elsewhere = "127.0.0.1:1".parse().unwrap();
        assert_eq!(owner_in(&open, listener, elsewhere), None);
        // A client that already closed is not vouched for, even with owner 0.
        let closed = table([
            line(listener, peer, "01", 1000, 7),
            line(peer, listener, "05", 0, 0),
        ]);
        assert_eq!(owner_in(&closed, peer, listener), None);
        let loopback = if cfg!(target_endian = "little") {
            "0100007F"
        } else {
            "7F000001"
        };
        assert_eq!(proc_address(peer), format!("{loopback}:9C40"));
    }

    #[tokio::test]
    async fn handlers_see_the_proxied_client_address() {
        use axum::{extract::ConnectInfo, routing::get, serve::ListenerExt};
        use tokio::io::AsyncWriteExt;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let cancellation = CancellationToken::new();
        let tap: fn(&mut TcpStream) = |_| {};
        let proxied = ProxyListener::spawn(listener, address, cancellation.clone()).tap_io(tap);
        let router = axum::Router::new().route(
            "/",
            get(|ConnectInfo(peer): ConnectInfo<SocketAddr>| async move { peer.to_string() }),
        );
        tokio::spawn(
            axum::serve(
                proxied,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .into_future(),
        );

        let mut stream = TcpStream::connect(address).await.unwrap();
        let addresses = [[100, 64, 0, 1], [100, 64, 0, 2]].concat();
        stream
            .write_all(&header(
                TCP_V4,
                &[addresses, vec![0x9c, 0x40, 0x01, 0xbb]].concat(),
            ))
            .await
            .unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: piqueld\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        cancellation.cancel();
        assert!(response.ends_with("100.64.0.1:40000"), "{response}");
    }

    #[tokio::test]
    async fn rejects_missing_truncated_and_unsupported_headers() {
        let mut local = header(TCP_V4, &[0; 12]);
        local[12] = 0x20;
        for input in [
            b"GET / HTTP/1.1\r\nHost: piqueld\r\n\r\n".to_vec(),
            header(TCP_V4, &[0; 8]),
            header(0x00, &[]),
            local,
            header(TCP_V4, &[0; 12])[..20].to_vec(),
        ] {
            assert!(
                ProxyListener::client(&mut input.as_slice()).await.is_err(),
                "accepted {input:?}"
            );
        }
    }
}
