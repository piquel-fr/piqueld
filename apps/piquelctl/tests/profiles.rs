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
        "NAME  ENDPOINT\ndev  /tmp/dev.sock\nprod  http://127.0.0.1:7845\n"
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
    assert_eq!(result.stdout, b"No profiles configured.\n");
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

#[test]
fn connection_failures_identify_the_effective_endpoint_source() {
    let fixture = ProfilesFixture::new();
    for (args, env, source, endpoint) in [
        (
            vec!["--profile", "testing", "status"],
            vec![],
            "profile \"testing\" in",
            "/tmp/profile-missing.sock",
        ),
        (
            vec!["--profile", "testing", "list"],
            vec![("PIQUELD_SOCKET", "/tmp/env-missing.sock")],
            "environment variable PIQUELD_SOCKET",
            "/tmp/env-missing.sock",
        ),
        (
            vec![
                "--profile",
                "testing",
                "--socket",
                "/tmp/flag-missing.sock",
                "--quiet",
                "--json",
                "status",
            ],
            vec![],
            "flag --socket",
            "/tmp/flag-missing.sock",
        ),
    ] {
        let output = fixture.run(&args, &env);
        assert_eq!(output.status.code(), Some(4));
        assert!(output.stdout.is_empty());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains(&format!("Endpoint source: {source}")),
            "{error}"
        );
        assert!(
            error.contains(&format!("Endpoint: Unix socket {endpoint}")),
            "{error}"
        );
        assert!(error.contains("Check the socket path"), "{error}");
        assert!(!error.contains("Timeout:"), "{error}");
    }
}

#[test]
fn timeout_provenance_is_independent_of_endpoint_provenance() {
    use std::os::unix::net::UnixListener;
    let fixture = ProfilesFixture::new();
    let socket = fixture.directory.path().join("pending.sock");
    let _listener = UnixListener::bind(&socket).unwrap();
    std::fs::write(
        fixture.directory.path().join("profiles.toml"),
        format!(
            "[profiles.testing]\nsocket = {:?}\ntimeout = \"20ms\"\n",
            socket.to_str().unwrap()
        ),
    )
    .unwrap();
    for (extra, env, source) in [
        (vec![], vec![], "profile \"testing\" in"),
        (
            vec![],
            vec![("PIQUELD_TIMEOUT", "20ms")],
            "environment variable PIQUELD_TIMEOUT",
        ),
        (
            vec!["--timeout", "20ms"],
            vec![("PIQUELD_TIMEOUT", "bad")],
            "flag --timeout",
        ),
    ] {
        let mut args = vec![
            "--profile",
            "testing",
            "--socket",
            socket.to_str().unwrap(),
            "status",
        ];
        args.extend(extra);
        let output = fixture.run(&args, &env);
        assert_eq!(output.status.code(), Some(4));
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("Endpoint source: flag --socket"), "{error}");
        assert!(error.contains("Timeout: 20ms"), "{error}");
        assert!(
            error.contains(&format!("Timeout source: {source}")),
            "{error}"
        );
    }
}

#[test]
fn invalid_configuration_reports_its_source_without_exposing_values() {
    let fixture = ProfilesFixture::new();
    let output = fixture.run(
        &[
            "--url",
            "http://user:private-token@localhost/?secret=value",
            "status",
        ],
        &[],
    );
    assert_eq!(output.status.code(), Some(2));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("Endpoint source: flag --url"), "{error}");
    assert!(!error.contains("private-token"));
    assert!(!error.contains("secret=value"));
    assert!(!error.contains("Endpoint:"));

    for malformed in [
        "[profiles.testing]\nurl = \"http://user:private-token@localhost/\" broken",
        "profiles = \"private-token\"",
    ] {
        std::fs::write(fixture.directory.path().join("profiles.toml"), malformed).unwrap();
        let output = fixture.run(&["--socket", "/tmp/override.sock", "status"], &[]);
        assert_eq!(output.status.code(), Some(2));
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("line ") && error.contains("column "),
            "{error}"
        );
        assert!(
            error.contains("profiles.toml (environment variable PIQUELD_PROFILES_FILE)"),
            "{error}"
        );
        assert!(!error.contains("private-token"));
        assert!(!error.contains("Endpoint:"));
    }
}

#[test]
fn refused_connections_identify_the_listening_endpoint_check() {
    let fixture = ProfilesFixture::new();
    let socket = fixture.directory.path().join("stopped.sock");
    drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let url = format!("http://127.0.0.1:{port}");
    let dns_url = format!("http://localhost.:{port}");
    for (flag, endpoint) in [
        ("--socket", socket.to_str().unwrap()),
        ("--url", url.as_str()),
        ("--url", dns_url.as_str()),
    ] {
        let output = fixture.run(&[flag, endpoint, "status"], &[]);
        assert_eq!(output.status.code(), Some(4));
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(endpoint), "{error}");
        assert!(
            error.contains("Check whether the daemon is listening"),
            "{error}"
        );
        assert!(!error.contains("Check the socket path"), "{error}");
        assert!(!error.contains("inspect the daemon logs"), "{error}");
    }
}
