use std::path::PathBuf;

use piquelcore::config;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct ClientConfig {
    #[serde(default = "config::defaults::socket_path")]
    pub socket_path: PathBuf,
}
