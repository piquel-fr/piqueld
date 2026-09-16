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
fn parse_failures_retain_the_underlying_toml_diagnostic() {
    let error = DaemonConfig::from_toml("[server]\nunknown_key = true").unwrap_err();
    let ConfigError::Parse(source) = &error else {
        panic!("expected a parse error, got {error:?}");
    };
    let rendered = source.to_string();
    assert!(
        rendered.contains("unknown field"),
        "diagnostic should name the offending field: {rendered}"
    );
    assert!(rendered.contains("unknown_key"));
    assert!(
        std::error::Error::source(&error).is_some(),
        "parse errors must retain their source"
    );
}

#[test]
fn built_in_defaults_are_valid() {
    assert!(DaemonConfig::validated_default().is_ok());
}

#[test]
fn retention_defaults_to_ten_days_and_accepts_zero_as_disabled() {
    assert_eq!(
        DaemonConfig::default().retention.finished_operation_days,
        10
    );
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
