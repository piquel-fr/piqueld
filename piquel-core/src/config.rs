use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum ConfigError {
    AlreadyLoaded,
    FileNotFound(PathBuf),
    ParseError(serde_json::Error),
    Validation(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::AlreadyLoaded => {
                write!(f, "Config has already been loaded")
            }
            ConfigError::FileNotFound(path) => {
                write!(f, "Config file {path:?} does not exist")
            }
            ConfigError::ParseError(e) => write!(f, "Failed to parse config: {e}"),
            ConfigError::Validation(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for ConfigError {}

pub trait Config: serde::de::DeserializeOwned {
    fn load(config_path: &Path) -> Result<Self, ConfigError> {
        let config_file = std::fs::read_to_string(config_path)
            .map_err(|_| ConfigError::FileNotFound(config_path.to_owned()))?;

        let mut config: Self =
            serde_json::from_str(&config_file).map_err(ConfigError::ParseError)?;

        // `set` fails only if another thread raced us — treat that as already loaded.
        config.validate()?;
        Ok(config)
    }
    fn validate(&mut self) -> Result<(), ConfigError>;
}

/// Returns the default socket path
pub fn default_socket_path() -> PathBuf {
    // TODO: rename to "/run/piqueld.sock" when we run as root
    PathBuf::from("/tmp/piqueld.sock")
}

/// Returns the default listen address
pub fn default_listen_addr() -> String {
    "0.0.0.0:7854".into()
}

/// Returns the default data dir
pub fn default_data_dir() -> String {
    "/var/lib/piqueld".into()
}
