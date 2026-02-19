use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub enum Command {
    Status,
    Reload,
    Stop,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Error(String),
}
