//! Child-process configuration shared by the CLI integration suites.
use std::process::Command;

pub fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_piquelctl"));
    for name in [
        "PIQUELD_PROFILE",
        "PIQUELD_PROFILES_FILE",
        "PIQUELD_SOCKET",
        "PIQUELD_URL",
        "PIQUELD_TIMEOUT",
    ] {
        command.env_remove(name);
    }
    command.env(
        "PIQUELD_PROFILES_FILE",
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/empty-profiles.toml"
        ),
    );
    command
}
