use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub enum Command {
    Echo(String),
    Status,
    Hostname,
    Reload,
    Stop,
}

impl std::fmt::Display for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Echo(msg) => {
                write!(f, "Echo \"{msg}\"")
            }
            Self::Status => {
                write!(f, "Status")
            }
            Self::Hostname => {
                write!(f, "Hostname")
            }
            Self::Reload => {
                write!(f, "Reload")
            }
            Self::Stop => {
                write!(f, "Stop")
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Message(String),
    Error(String),
}
