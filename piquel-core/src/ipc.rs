pub mod client;
pub mod message;
pub mod server;

#[derive(Clone, Copy)]
pub enum ConnectionType {
    Tcp,
    Uds,
}
