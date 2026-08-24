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
fn host_paths_and_loopback_listener_are_validated() {
    for document in [
        "[server]\nhttp_listen = '0.0.0.0:7845'",
        "[server]\nhttp_listen = '127.0.0.1:0'",
        "[server]\ndata_dir = 'relative/state'",
        "[server]\ndata_dir = '/'",
        "[docker]\nsocket = 'relative.sock'",
        "[database]\npath = '/tmp/piqueld.db'",
        "[reconciliation]\nmax_parallel_operations = 0",
        "[reconciliation]\nscan_interval_seconds = 0",
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
fn omitted_http_listener_disables_tcp_and_defaults_stay_enabled() {
    let disabled = DaemonConfig::from_toml("[server]\ndata_dir = '/tmp/p'").unwrap();
    assert!(disabled.server.http_listen.is_none());
    assert_eq!(
        DaemonConfig::default().server.http_listen,
        Some("127.0.0.1:7845".parse().expect("constant socket address"))
    );
}

#[test]
fn derived_paths_live_inside_the_data_directory() {
    let config = DaemonConfig::from_toml("[server]\ndata_dir = '/srv/piqueld'").unwrap();
    assert_eq!(
        config.server.socket_path(),
        PathBuf::from("/srv/piqueld/piqueld.sock")
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
