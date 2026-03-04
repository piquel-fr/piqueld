use std::io;

use serde::{Deserialize, Serialize};

// TODO: rename to "/run/piqueld.sock" when we run as root
pub const SOCKET_PATH: &str = "/tmp/piqueld.sock";
pub const LISTEN_ADDR: &str = "0.0.0.0:7854";

pub const DATA_DIR: &str = "/var/lib/piqueld";

/// Settings for how to load configuration. This is mainly to accomodate for
/// server and CLI configuration.
///
///
pub struct ConfigLoadSetting {}

#[derive(Serialize, Deserialize)]
pub struct Config {}

impl Config {
    fn load() -> io::Result<Self> {
        Ok(Config {})
    }
}
