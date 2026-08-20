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
