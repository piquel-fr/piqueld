pub mod client;
pub mod message;

#[derive(Clone, Copy)]
pub enum ConnectionType {
    Tcp,
    Uds,
}
