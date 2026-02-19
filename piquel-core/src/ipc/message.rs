use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

pub trait Stream: Read + Write {}

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
