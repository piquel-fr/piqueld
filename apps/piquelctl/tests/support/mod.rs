//! Child-process configuration shared by the CLI integration suites.
use std::process::Command;

/// Builds a `piquelctl` command isolated from inherited connection,
/// profile, and login configuration.
pub fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_piquelctl"));
    for name in [
        "PIQUELD_PROFILE",
        "PIQUELD_SOCKET",
        "PIQUELD_URL",
        "PIQUELD_TIMEOUT",
        "PIQUELD_TOKEN",
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
    // Absent on purpose: a missing credentials file means no saved login.
    command.env(
        "PIQUELD_CREDENTIALS_FILE",
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/missing-credentials.json"
        ),
    );
    command
}
