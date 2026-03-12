pub mod client;
pub mod message;

#[derive(Debug, Clone, Copy)]
pub enum ConnectionType {
    Tcp,
    Uds,
}

impl std::fmt::Display for ConnectionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp => {
                write!(f, "TCP")
            }
            Self::Uds => {
                write!(f, "UDS")
            }
        }
    }
}
