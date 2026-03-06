use std::path::PathBuf;

pub fn socket_path() -> PathBuf {
    // TODO: rename to "/run/piqueld.sock" when we run as root
    PathBuf::from("/etc/piqueld/config.json")
}

pub fn listen_addr() -> String {
    String::from("0.0.0.0:7854")
}

/// Returns the default data dir
pub fn data_dir() -> PathBuf {
    PathBuf::from("/var/lib/piqueld")
}

pub const SERVER_CONFIG_PATH: &str = "/etc/piqueld/config.json";

pub fn client_config_path() -> PathBuf {
    std::env::home_dir()
        .unwrap()
        .join(".config/piquel/config.json")
}
