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
        Err(ConfigError::Parse(_))
    ));
}

#[test]
fn host_paths_and_listener_settings_are_validated() {
    for document in [
        "[server]\nhttp_listen = '0.0.0.0:7845'",
        "[server]\nport = 0",
        "[server]\nallowed_hosts = ['*.example.com']",
        "[server]\nallowed_hosts = ['http://example.com']",
        "[server]\nallowed_hosts = ['example.com:7845']",
        "[server]\nallowed_hosts = ['']",
        "[server]\nlisten_mode = 'all'",
        "[server]\ndata_dir = 'relative/state'",
        "[server]\ndata_dir = '/'",
        "[server]\nruntime_dir = 'relative/run'",
        "[server]\nruntime_dir = '/'",
        "[server]\ndata_dir = '/run/piqueld'",
        "[docker]\nsocket = 'relative.sock'",
        "[database]\npath = '/tmp/piqueld.db'",
        "[reconciliation]\nscan_interval_seconds = 0",
        "[reconciliation]\nscan_interval_seconds = 86401",
        "[reconciliation]\nprepare_timeout_seconds = 86401",
        "[reconciliation]\nconvergence_timeout_seconds = 86401",
    ] {
        assert!(
            matches!(
                DaemonConfig::from_toml(document),
                Err(ConfigError::Parse(_) | ConfigError::Invalid(_))
            ),
            "accepted {document}"
        );
    }
}

#[test]
fn tcp_defaults_to_off_with_or_without_a_server_table() {
    for source in ["", "[server]", "[server]\ndata_dir = '/tmp/p'"] {
        let config = DaemonConfig::from_toml(source).unwrap();
        assert_eq!(config.server.listen_mode, ListenMode::Off);
        assert_eq!(config.server.port, 7845);
    }
    for mode in ["off", "localhost", "tailscale", "both"] {
        let config =
            DaemonConfig::from_toml(&format!("[server]\nlisten_mode = '{mode}'\nport = 8443"))
                .unwrap();
        assert_eq!(config.server.listen_mode.to_string(), mode);
        assert_eq!(config.server.port, 8443);
    }
}

#[test]
fn socket_and_database_use_separate_directories() {
    let config = DaemonConfig::from_toml("[server]\ndata_dir = '/srv/piqueld'").unwrap();
    assert_eq!(
        config.server.socket_path(),
        PathBuf::from("/run/piqueld/piqueld.sock")
    );
    assert_eq!(
        config.server.database_path(),
        PathBuf::from("/srv/piqueld/piqueld.db")
    );
}

#[test]
fn removed_data_directory_is_not_accepted_as_configuration() {
    assert!(matches!(
        DaemonConfig::from_toml("data_dir = '/tmp/piqueld'"),
        Err(ConfigError::Parse(_))
    ));
}

#[test]
fn parse_failures_keep_the_diagnostic_but_not_the_source_line() {
    let error = DaemonConfig::from_toml(
        "[server]\nport = 7845\n\n[[notifications.destinations]]\nname = 'ops'\nurl = 'https://hooks.example/secret-token'\nunknown_key = true",
    )
    .unwrap_err();
    let ConfigError::Parse(diagnostic) = &error else {
        panic!("expected a parse error, got {error:?}");
    };
    assert!(
        diagnostic.message.contains("unknown field") && diagnostic.message.contains("unknown_key"),
        "diagnostic should name the offending field: {diagnostic}"
    );
    assert_eq!(
        diagnostic.location,
        Some(piqueld_core::TomlLocation { line: 7, column: 1 })
    );
    let rendered = format!("{:?}", anyhow::Error::new(error));
    assert!(
        !rendered.contains("secret-token"),
        "diagnostic must not echo configuration source: {rendered}"
    );
}

#[test]
fn built_in_defaults_are_valid() {
    assert!(DaemonConfig::validated_default().is_ok());
}

#[test]
fn retention_defaults_are_bounded_and_accept_zero_as_disabled() {
    let defaults = DaemonConfig::default().retention;
    assert_eq!(defaults.finished_operation_days, 10);
    assert_eq!(defaults.event_days, 90);
    assert_eq!(defaults.daemon_event_days, 90);
    let disabled = DaemonConfig::from_toml("[retention]\nfinished_operation_days = 0").unwrap();
    assert_eq!(disabled.retention.finished_operation_days, 0);
    let configured = DaemonConfig::from_toml("[retention]\nfinished_operation_days = 30").unwrap();
    assert_eq!(configured.retention.finished_operation_days, 30);
    assert!(matches!(
        DaemonConfig::from_toml("[retention]\nunknown = 'x'"),
        Err(ConfigError::Parse(_))
    ));
}

#[test]
fn runtime_directory_override_changes_only_the_socket_path() {
    let config = DaemonConfig::from_toml("[server]\nruntime_dir = '/tmp/piqueld-dev-run'").unwrap();
    assert_eq!(
        config.server.socket_path(),
        PathBuf::from("/tmp/piqueld-dev-run/piqueld.sock")
    );
    assert_eq!(
        config.server.database_path(),
        PathBuf::from("/var/lib/piqueld/piqueld.db")
    );
}

#[test]
fn tailscale_node_settings_are_validated() {
    let config = DaemonConfig::from_toml(
        "[tailscale]\nhostname = 'piqueld-dev'\nauth_key_file = '/run/credentials/ts-auth-key'",
    )
    .unwrap();
    assert_eq!(config.tailscale.hostname, "piqueld-dev");
    assert_eq!(
        config.server.tailscale_dir(),
        PathBuf::from("/var/lib/piqueld/tailscale")
    );
    assert_eq!(config.public_url(), "http://localhost:7845");
    for document in [
        "[tailscale]\nhostname = ''",
        "[tailscale]\nhostname = 'piqueld.example'",
        "[tailscale]\nhostname = '-piqueld'",
        "[tailscale]\nauth_key_file = 'relative/key'",
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
