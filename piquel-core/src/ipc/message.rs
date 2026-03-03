use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub enum Command {
    Echo(String),
    Status,
    Hostname,
    Reload,
    Stop,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Message(String),
    Error(String),
}
