use std::path::PathBuf;

use piquelcore::config::{self, Config};
use serde::{self, Deserialize};

#[derive(Deserialize)]
pub struct ServerConfig {
    #[serde(default = "config::defaults::data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "config::defaults::socket_path")]
    pub socket: PathBuf,
    #[serde(default = "config::defaults::listen_addr")]
    pub address: String,
    #[serde(default = "config::defaults::listen_port")]
    pub port: u16,
}

impl Config for ServerConfig {
    fn validate(&mut self) -> Result<(), config::ConfigError> {
        Ok(())
    }
}
