use super::*;

#[test]
fn empty_document_uses_documented_defaults() {
    assert_eq!(
        DaemonConfig::from_toml("").unwrap(),
        DaemonConfig::default()
    );
}

#[test]
fn unknown_sections_are_rejected() {
    assert!(matches!(
        DaemonConfig::from_toml("[extra]\nvalue = 'x'"),
        Err(ConfigError::Parse)
    ));
}

#[test]
fn host_paths_and_loopback_listener_are_validated() {
    for document in [
        "[server]\nhttp_listen = '0.0.0.0:7845'",
        "[server]\nhttp_listen = '127.0.0.1:0'",
        "[server]\nunix_socket = 'relative.sock'",
        "[database]\npath = 'relative.db'",
        "[server]\nui_dir = 'relative-ui'",
        "[docker]\nsocket = 'relative.sock'",
        "[reconciliation]\nmax_parallel_operations = 0",
        "[reconciliation]\nscan_interval_seconds = 0",
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
fn removed_data_directory_is_not_accepted_as_configuration() {
    assert!(matches!(
        DaemonConfig::from_toml("data_dir = '/tmp/piqueld'"),
        Err(ConfigError::Parse)
    ));
}

#[test]
fn built_in_defaults_are_valid() {
    assert!(DaemonConfig::validated_default().is_ok());
}

#[test]
fn trusted_identity_and_browser_origins_fail_closed() {
    assert!(matches!(
        DaemonConfig::from_toml("[security]\ntrust_tailscale_headers = true"),
        Err(ConfigError::Invalid(_))
    ));
    let trusted = DaemonConfig::from_toml(
        "[security]\ntrusted_loopback_proxy = true\ntrust_tailscale_headers = true\nallowed_origins = ['https://admin.example']",
    );
    assert!(trusted.is_ok());
    for origin in ["*", "https://example.test/path", "file:///tmp/x"] {
        let source = format!("[security]\nallowed_origins = ['{origin}']");
        assert!(matches!(
            DaemonConfig::from_toml(&source),
            Err(ConfigError::Invalid(_))
        ));
    }
}
