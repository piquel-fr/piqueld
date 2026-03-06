use std::path::PathBuf;

use piquelcore::config::{self, Config};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct ClientConfig {
    #[serde(default = "config::defaults::socket_path")]
    pub socket_path: PathBuf,
}

impl Config for ClientConfig {
    fn validate(&mut self) -> Result<(), config::ConfigError> {
        Ok(())
    }
}
