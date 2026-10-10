//! Child-process configuration shared by the CLI integration suites.
use serde_json::Value;
use std::process::{Command, Output};

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

/// The one error event of a `--json` failure, checking that every stderr
/// line is a JSON event and that stdout is empty.
pub fn json_error(output: &Output) -> Value {
    assert!(!output.status.success());
    assert_eq!(output.stdout, b"", "a failure writes nothing on stdout");
    let events: Vec<Value> = String::from_utf8_lossy(&output.stderr)
        .lines()
        .map(|line| serde_json::from_str(line).expect("every stderr line is a JSON event"))
        .collect();
    let mut errors = events.iter().filter_map(|event| event.get("error"));
    let error = errors.next().expect("an error event").clone();
    assert!(errors.next().is_none(), "exactly one error event");
    error
}
