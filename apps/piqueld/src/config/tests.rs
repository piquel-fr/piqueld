use super::*;
use std::fs;

#[test]
fn empty_document_uses_documented_defaults() {
    assert_eq!(
        DaemonConfig::from_toml("").unwrap(),
        DaemonConfig::default()
    );
}

#[test]
fn malformed_toml_is_rejected() {
    assert!(matches!(
        DaemonConfig::from_toml("[server"),
        Err(ConfigError::Parse)
    ));
}

#[test]
fn unknown_fields_are_rejected() {
    assert!(matches!(
        DaemonConfig::from_toml("applications = []"),
        Err(ConfigError::Parse)
    ));
}

#[test]
fn invalid_listen_socket_and_registry_settings_are_rejected() {
    for document in [
        "[server]\nhttp_listen = '0.0.0.0:7845'",
        "[server]\nhttp_listen = '127.0.0.1:0'",
        "[server]\nunix_socket = 'relative.sock'",
        "[docker]\nsocket = 'relative.sock'",
        "[registry]\naddress = '192.0.2.1:5000'",
        "[registry]\naddress = '127.0.0.1:0'",
        "[registry]\ndata_dir = 'registry'",
    ] {
        assert!(
            matches!(
                DaemonConfig::from_toml(document),
                Err(ConfigError::Invalid(_))
            ),
            "accepted {document}"
        );
    }
}

#[test]
fn parse_errors_do_not_echo_mistaken_inline_secrets() {
    let secret = "super-sensitive-inline-value";
    let document =
        format!("[credentials.encryption_key]\nsource = 'file'\npath = '/key'\nvalue = '{secret}'");
    let error = DaemonConfig::from_toml(&document).unwrap_err();

    assert!(!format!("{error:?}").contains(secret));
    assert!(!error.to_string().contains(secret));
}

#[test]
fn unsafe_systemd_credential_names_are_rejected() {
    let oversized = "x".repeat(256);
    for name in ["", ".", "..", "nested/key", &oversized] {
        let document =
            format!("[credentials.encryption_key]\nsource = 'systemd_credential'\nname = '{name}'");
        assert!(matches!(
            DaemonConfig::from_toml(&document),
            Err(ConfigError::Invalid(_))
        ));
    }
    assert!(matches!(
        (CredentialReference::SystemdCredential {
            name: "contains\0nul".into()
        })
        .validate(),
        Err(ConfigError::Invalid(_))
    ));
}

#[test]
fn encryption_key_files_in_the_nix_store_are_rejected() {
    assert!(matches!(
        DaemonConfig::from_toml(
            "[credentials.encryption_key]\nsource = 'file'\npath = '/nix/store/example-master-key'"
        ),
        Err(ConfigError::Invalid(_))
    ));
}

#[test]
fn invalid_limits_are_rejected() {
    assert!(matches!(
        DaemonConfig::from_toml("[reconciliation]\nmax_parallel_builds = 0"),
        Err(ConfigError::Invalid(_))
    ));
    assert!(matches!(
        DaemonConfig::from_toml("[traefik]\norigin_port = 0"),
        Err(ConfigError::Invalid(_))
    ));
}

#[test]
fn loading_is_read_only_and_credential_paths_are_redacted() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("piqueld.toml");
    let source = "[credentials.encryption_key]\nsource = 'file'\npath = '/run/credentials/piqueld/master-key'\n";
    fs::write(&path, source).unwrap();
    let before = fs::read(&path).unwrap();
    let config = DaemonConfig::load(&path).unwrap();
    assert_eq!(fs::read(&path).unwrap(), before);
    let debug = format!("{config:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("master-key"));
}
