//! Profiles exercise precedence through the real command without changing process globals.
use std::process::{Command, Output};
struct ProfilesFixture {
    directory: tempfile::TempDir,
}
impl ProfilesFixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("profiles.toml"),
            r#"
[profiles.testing]
socket = "/tmp/profile-missing.sock"
timeout = "2s"
[profiles.invalid]
socket = "/tmp/invalid.sock"
url = "http://127.0.0.1:7845"
"#,
        )
        .unwrap();
        Self { directory }
    }
    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
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
        command
            .args([
                "--profiles-file",
                self.directory
                    .path()
                    .join("profiles.toml")
                    .to_str()
                    .unwrap(),
            ])
            .args(args)
            .envs(env.iter().copied())
            .output()
            .unwrap()
    }
}
#[test]
fn profile_and_environment_selection_report_configuration_errors() {
    let fixture = ProfilesFixture::new();
    for (args, env, expected) in [
        (
            vec!["--profile", "missing", "status"],
            vec![],
            "Unknown connection profile",
        ),
        (
            vec!["status"],
            vec![("PIQUELD_PROFILE", "missing")],
            "Unknown connection profile",
        ),
        (
            vec!["--profile", "invalid", "status"],
            vec![],
            "exactly one socket or URL",
        ),
        (
            vec!["status"],
            vec![
                ("PIQUELD_SOCKET", "/tmp/missing"),
                ("PIQUELD_URL", "http://127.0.0.1"),
            ],
            "Set only one",
        ),
        (
            vec!["--profile", "testing", "status"],
            vec![("PIQUELD_TIMEOUT", "bad")],
            "timeout must be",
        ),
    ] {
        let result = fixture.run(&args, &env);
        assert_eq!(result.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&result.stderr).contains(expected));
    }
}
#[test]
fn explicit_transport_and_timeout_override_environment_and_profile() {
    let fixture = ProfilesFixture::new();
    let result = fixture.run(
        &[
            "--profile",
            "testing",
            "--socket",
            "/tmp/explicit-missing.sock",
            "--timeout",
            "1s",
            "status",
        ],
        &[
            ("PIQUELD_SOCKET", "/tmp/env.sock"),
            ("PIQUELD_URL", "invalid"),
            ("PIQUELD_TIMEOUT", "invalid"),
        ],
    );
    // Resolution succeeds; the deliberately absent socket is a transport error.
    assert_eq!(
        result.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let result = fixture.run(
        &["--profile", "testing", "status"],
        &[("PIQUELD_URL", "invalid")],
    );
    // Environment URL replaced the profile socket and is now validated as a URL.
    assert_eq!(result.status.code(), Some(2));
}
