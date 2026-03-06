use std::path::PathBuf;

use piquelcore::config::{self, Config};
use serde::{self, Deserialize};

#[derive(Deserialize)]
pub struct ServerConfig {
    #[serde(default = "config::default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "config::default_socket_path")]
    pub socket_path: PathBuf,
    #[serde(default = "config::default_listen_addr")]
    pub listen_addr: String,
}

impl Config for ServerConfig {
    fn validate(&mut self) -> Result<(), config::ConfigError> {
        Ok(())
    }
}

pub const DEFAULT_CONFIG_PATH: &str = "/etc/piqueld/config.json";
