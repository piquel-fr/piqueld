use std::path::PathBuf;

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
    pub docker: DockerConfig,
}

impl Config for ServerConfig {
    fn validate(&mut self) -> Result<(), config::ConfigError> {
        Ok(())
    }
}

#[derive(Deserialize)]
pub struct DockerConfig {
    pub jobs: JobsConfig,
}

#[derive(Deserialize)]
pub struct JobsConfig {
    pub app: JobConfig<AppConfig>,
}

#[derive(Deserialize)]
pub struct JobInfo {
    /// The Swarm Node roles that this job will be run on
    pub roles: Vec<String>,
}

/// Generic config struct for job configuration.
#[derive(Deserialize)]
pub struct JobConfig<T> {
    pub info: JobInfo,
    pub config: T,
}

/// Placeholder job type.
#[derive(Deserialize)]
pub struct AppConfig {}
