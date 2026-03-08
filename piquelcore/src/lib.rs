pub mod config;
pub mod ipc;
pub mod logging;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
