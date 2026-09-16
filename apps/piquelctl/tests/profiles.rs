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
"#,
        )
        .unwrap();
        Self { directory }
    }
    fn command(&self) -> Command {
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
            self.directory.path().join("profiles.toml"),
        );
        command
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        self.command()
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

#[test]
fn profiles_list_names_and_endpoints_without_resolving_a_connection() {
    let fixture = ProfilesFixture::new();
    std::fs::write(
        fixture.directory.path().join("profiles.toml"),
        r#"
[profiles.prod]
url = "http://127.0.0.1:7845"
[profiles.dev]
socket = "/tmp/dev.sock"
"#,
    )
    .unwrap();
    let env = [
        ("PIQUELD_PROFILE", "missing"),
        ("PIQUELD_SOCKET", "/tmp/unused.sock"),
        ("PIQUELD_URL", "invalid"),
        ("PIQUELD_TIMEOUT", "invalid"),
    ];
    let result = fixture.run(
        &["--profile", "also-missing", "--url", "invalid", "profiles"],
        &env,
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        String::from_utf8(result.stdout).unwrap(),
        "NAME\tENDPOINT\ndev\t/tmp/dev.sock\nprod\thttp://127.0.0.1:7845\n"
    );
    let result = fixture.run(&["profiles", "--json", "--quiet"], &env);
    assert!(result.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap(),
        serde_json::json!({
            "profiles": [
                {"name": "dev", "endpoint": "/tmp/dev.sock"},
                {"name": "prod", "endpoint": "http://127.0.0.1:7845"}
            ]
        })
    );
    let result = fixture.run(&["profiles", "--quiet"], &env);
    assert!(result.status.success());
    assert!(result.stdout.is_empty());
}

#[test]
fn explicit_file_flag_replaces_environment_file_and_automatic_discovery() {
    let fixture = ProfilesFixture::new();
    let file = fixture.directory.path().join("explicit.toml");
    std::fs::write(&file, "[profiles.explicit]\nurl = 'http://localhost:7845'").unwrap();
    // Neither the default environment-selected file nor the user file contributes entries.
    let user_directory = fixture.directory.path().join("piqueld");
    std::fs::create_dir(&user_directory).unwrap();
    std::fs::write(user_directory.join("profiles.toml"), "invalid TOML").unwrap();
    let env = [(
        "XDG_CONFIG_HOME",
        fixture.directory.path().to_str().unwrap(),
    )];
    let result = fixture.run(
        &[
            "profiles",
            "--json",
            "--profiles-file",
            file.to_str().unwrap(),
        ],
        &env,
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap(),
        serde_json::json!({
            "profiles": [{"name": "explicit", "endpoint": "http://localhost:7845"}]
        })
    );
    let result = fixture.run(&["profiles", "--json"], &env);
    assert!(result.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap()["profiles"][0]["name"],
        "testing"
    );
}

#[test]
fn empty_profiles_succeed_and_missing_explicit_files_fail() {
    let fixture = ProfilesFixture::new();
    let path = fixture.directory.path().join("profiles.toml");
    std::fs::write(&path, "").unwrap();
    let result = fixture.run(&["profiles", "--json"], &[]);
    assert!(result.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap(),
        serde_json::json!({"profiles": []})
    );
    let result = fixture.run(&["profiles"], &[]);
    assert!(result.status.success());
    assert!(result.stdout.is_empty());
    std::fs::remove_file(&path).unwrap();
    let result = fixture.run(&["profiles"], &[]);
    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains(path.to_str().unwrap()));
}

#[test]
fn listing_rejects_invalid_profiles_even_when_not_selected() {
    let fixture = ProfilesFixture::new();
    let path = fixture.directory.path().join("profiles.toml");
    std::fs::write(
        &path,
        "[profiles.broken]\nsocket = '/tmp/dev.sock'\nurl = 'http://localhost'",
    )
    .unwrap();
    let result = fixture.run(&["profiles"], &[]);
    assert_eq!(result.status.code(), Some(2));
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(error.contains("broken") && error.contains(path.to_str().unwrap()));
}

#[test]
fn automatic_discovery_uses_xdg_then_falls_back_to_home() {
    let fixture = ProfilesFixture::new();
    let root = fixture.directory.path();
    for (directory, endpoint) in [
        (root.join("piqueld"), "/tmp/xdg.sock"),
        (root.join(".config/piqueld"), "/tmp/home.sock"),
    ] {
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("profiles.toml"),
            format!("[profiles.discovery_test]\nsocket = '{endpoint}'"),
        )
        .unwrap();
    }
    for xdg in [Some(root.as_os_str()), Some(std::ffi::OsStr::new("")), None] {
        let mut command = fixture.command();
        command
            .env_remove("PIQUELD_PROFILES_FILE")
            .env("HOME", root)
            .env_remove("XDG_CONFIG_HOME");
        if let Some(xdg) = xdg {
            command.env("XDG_CONFIG_HOME", xdg);
        }
        let result = command.args(["profiles", "--json"]).output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let output: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        let profile = output["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .find(|profile| profile["name"] == "discovery_test")
            .unwrap();
        assert_eq!(
            profile["endpoint"],
            if xdg.is_some_and(|value| !value.is_empty()) {
                "/tmp/xdg.sock"
            } else {
                "/tmp/home.sock"
            }
        );
    }
}
