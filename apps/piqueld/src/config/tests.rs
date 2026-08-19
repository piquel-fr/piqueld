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
        "[docker]\nsocket = 'relative.sock'",
        "[reconciliation]\nmax_parallel_operations = 0",
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
