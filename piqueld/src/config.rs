use std::{collections::HashMap, path::PathBuf};

use piquel::config::{self, Config};
use serde::{self, Deserialize};

#[derive(Deserialize)]
pub struct ServerConfig {
    #[serde(default = "config::defaults::data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "config::defaults::socket_path")]
    pub socket: PathBuf,
    #[serde(default = "config::defaults::listen_addr")]
    pub address: String,
    #[serde(default = "config::defaults::port")]
    pub port: u16,
    #[serde(default)]
    pub docker: DockerConfig,
}

impl Config for ServerConfig {
    fn validate(&mut self) -> Result<(), config::ConfigError> {
        Ok(())
    }
}

#[derive(Deserialize)]
pub struct DockerConfig {
    /// When starting a docker service of the specified type, will force the
    /// service onto a Swarm node with the specified role label.
    /// If no roles are specified, will run on any node.
    pub roles: HashMap<ServiceType, Vec<String>>,
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            roles: HashMap::new(),
        }
    }
}

#[derive(Eq, PartialEq, Deserialize, Hash)]
pub enum ServiceType {
    /// Any process that needs storage.
    Storage,
}
