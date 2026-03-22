use std::path::PathBuf;

use piquel::config::{self, Config, ConfigError};
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
    pub update: UpdateConfig,
}

impl Config for ServerConfig {
    fn validate(&mut self) -> Result<(), ConfigError> {
        if self.update.enable {
            let update = &mut self.update;
            if update.command.is_none() {
                return Err(ConfigError::Validation(String::from(
                    "Update command must be specified",
                )));
            }

            if let Some(repo) = &update.repository {
                let (owner, name) = match repo.split_once("/") {
                    Some(tuple) => tuple,
                    None => {
                        return Err(ConfigError::Validation(
                            "Repository name {repo} is invalid".to_string(),
                        ));
                    }
                };

                update.repo_split = Some((owner.to_string(), name.to_string()));
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
pub struct UpdateConfig {
    pub enable: bool,
    pub command: Option<String>,
    pub repository: Option<String>,
    #[serde(skip)]
    pub repo_split: Option<(String, String)>,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            enable: false,
            command: None,
            repository: None,
            repo_split: None,
        }
    }
}
