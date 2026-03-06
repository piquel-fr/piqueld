use std::path::PathBuf;

use piquelcore::config::{self, Config};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct ClientConfig {
    #[serde(default = "config::defaults::socket_path")]
    pub socket: PathBuf,
    #[serde(default = "config::defaults::listen_addr")]
    pub address: String,
    #[serde(default = "config::defaults::listen_port")]
    pub port: u16,

    pub default_to_tcp: bool,
}

impl Config for ClientConfig {
    fn validate(&mut self) -> Result<(), config::ConfigError> {
        Ok(())
    }
}
