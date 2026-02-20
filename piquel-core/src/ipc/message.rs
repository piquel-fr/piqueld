use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

pub trait Stream: Read + Write {}

#[derive(Debug, Serialize, Deserialize)]
pub enum Command {
    Echo(String),
    Status,
    Reload,
    Stop,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Message(String),
    Error(String),
}
