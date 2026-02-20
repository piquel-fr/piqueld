pub mod client;
pub mod message;
pub mod server;

pub type ServerType = ClientType;

#[derive(Clone, Copy)]
pub enum ClientType {
    TcpClient,
    UdsClient,
}
