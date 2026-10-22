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
        "[server]\nport = 7845\n\n[auth]\nmax_token_days = 90\npublic_url = 'https://secret-token.example.com'\nunknown_key = true",
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
fn preview_bounds_must_be_values_a_manifest_could_set() {
    let previews = DaemonConfig::from_toml("[previews]\nmax_total = 0\nmax_replicas = 2")
        .unwrap()
        .previews;
    assert_eq!((previews.max_total, previews.max_replicas), (0, 2));
    for invalid in [
        "default_cpu_millis = 0",
        "default_memory_bytes = 0",
        "max_replicas = 0",
        "max_replicas = 101",
    ] {
        assert!(
            matches!(
                DaemonConfig::from_toml(&format!("[previews]\n{invalid}")),
                Err(ConfigError::Invalid(_))
            ),
            "{invalid}"
        );
    }
}

#[test]
fn snapshot_uploads_and_checkouts_need_nonzero_limits() {
    let config = DaemonConfig::from_toml("[snapshots]\nmax_upload_bytes = 4096").unwrap();
    assert_eq!(config.snapshots.max_upload_bytes.get(), 4096);
    assert!(DaemonConfig::from_toml("[snapshots]\nmax_upload_bytes = 0").is_err());
    let config = DaemonConfig::from_toml("[checkouts]\nmax_bytes = 4096").unwrap();
    assert_eq!(config.checkouts.max_bytes.get(), 4096);
    assert!(DaemonConfig::from_toml("[checkouts]\nmax_bytes = 0").is_err());
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
    assert_eq!(config.tailscale.https_port.get(), 443);
    let shared = DaemonConfig::from_toml(
        "[tailscale]\nsocket = '/tmp/piqueld-dev/tailnet/tailscaled.sock'\nhttps_port = 7846",
    )
    .unwrap();
    assert_eq!(shared.tailscale.https_port.get(), 7846);
    for document in [
        "[tailscale]\nhostname = ''",
        "[tailscale]\nhostname = 'piqueld.example'",
        "[tailscale]\nhostname = '-piqueld'",
        "[tailscale]\nhttps_port = 0",
        "[tailscale]\nsocket = 'tailscaled.sock'",
        "[tailscale]\nsocket = '/run/ts.sock'\nauth_key_file = '/run/credentials/ts-auth-key'",
    ] {
        assert!(
            DaemonConfig::from_toml(document).is_err(),
            "accepted {document}"
        );
    }
}

#[test]
fn the_dashboard_hostname_needs_private_ingress_and_becomes_the_public_url() {
    let private = "[ingress]\nenabled = true\n[ingress.private]\nenabled = true\n";
    let config = DaemonConfig::from_toml(&format!(
        "{private}dashboard_hostname = 'dashboard.example.com'"
    ))
    .unwrap();
    assert_eq!(config.public_url(), "https://dashboard.example.com");
    let explicit = DaemonConfig::from_toml(&format!(
        "[auth]\npublic_url = 'https://piqueld.example.com'\n{private}dashboard_hostname = 'dashboard.example.com'"
    ))
    .unwrap();
    assert_eq!(explicit.public_url(), "https://piqueld.example.com");
    for document in [
        "[ingress.private]\nenabled = true\ndashboard_hostname = 'dashboard.example.com'".into(),
        "[ingress]\nenabled = true\n[ingress.private]\ndashboard_hostname = 'dashboard.example.com'".into(),
        format!("{private}dashboard_hostname = '*.example.com'"),
        "[ingress]\nenabled = true\nwebhook_hostname = 'dashboard.example.com'\n\
         [ingress.private]\nenabled = true\ndashboard_hostname = 'dashboard.example.com'"
            .into(),
    ] {
        assert!(
            DaemonConfig::from_toml(&document).is_err(),
            "accepted {document}"
        );
    }
}

#[test]
fn credential_files_are_read_and_never_shown() {
    let directory = tempfile::tempdir().unwrap();
    let headers = directory.path().join("otlp-headers");
    let auth_key = directory.path().join("ts-auth-key");
    std::fs::write(&headers, "authorization: Bearer secret\n").unwrap();
    let source = format!(
        "[tailscale]\nauth_key_file = '{}'\n\
         [telemetry]\nendpoint = 'unix:///run/otel/otlp.sock'\nheaders_file = '{}'",
        auth_key.display(),
        headers.display()
    );
    let config = DaemonConfig::from_toml(&source).unwrap();
    assert_eq!(
        config.telemetry.headers.as_ref().unwrap().expose(),
        "authorization: Bearer secret"
    );
    let view = format!("{:?} {config:?}", config.view());
    assert!(
        view.contains(&format!("from {}", headers.display())),
        "{view}"
    );
    assert!(
        view.contains(&format!("from {}", auth_key.display())),
        "{view}"
    );
    assert!(!view.contains("secret"), "{view}");
}

#[test]
fn dns_providers_accept_credentials_only_from_files() {
    let directory = tempfile::tempdir().unwrap();
    for name in ["cf", "ak", "as", "ck"] {
        std::fs::write(directory.path().join(name), format!("{name}-value\n")).unwrap();
    }
    let path = |name: &str| directory.path().join(name).display().to_string();
    let source = format!(
        "[ingress.acme]\nemail = 'admin@example.com'\n\
         [ingress]\npublic_addresses = ['203.0.113.10', '2001:db8::10']\n\
         [[dns.providers]]\nkind = 'cloudflare'\napi_token_file = '{}'\nmanage_records = true\n\
         [[dns.providers]]\nkind = 'ovh'\nendpoint = 'ovh-eu'\napplication_key_file = '{}'\n\
         application_secret_file = '{}'\nconsumer_key_file = '{}'",
        path("cf"),
        path("ak"),
        path("as"),
        path("ck")
    );
    let config = DaemonConfig::from_toml(&source).unwrap();
    assert_eq!(
        config
            .dns
            .providers
            .iter()
            .map(|config| (config.provider.kind(), config.manage_records))
            .collect::<Vec<_>>(),
        [("cloudflare", true), ("ovh", false)]
    );
    let view = config.view();
    let dns = &view.groups["DNS providers"];
    assert_eq!(
        dns["1. cloudflare"],
        format!("API token from {}; manages route records", path("cf"))
    );
    assert!(
        dns["2. ovh"].starts_with("ovh-eu, application key from")
            && dns["2. ovh"].ends_with("; route records are manual"),
        "{dns:?}"
    );
    assert_eq!(view.groups["Ingress"]["ACME email"], "admin@example.com");
    assert_eq!(
        view.groups["Ingress"]["Public addresses"],
        "203.0.113.10, 2001:db8::10"
    );
    let shown = format!("{view:?} {config:?}");
    assert!(!shown.contains("-value"), "{shown}");

    for invalid in [
        "[[dns.providers]]\nkind = 'cloudflare'\napi_token = 'inline'",
        "[[dns.providers]]\nkind = 'route53'",
        "[[dns.providers]]\nkind = 'cloudflare'\napi_token_file = '/x'\nmanage_record = true",
        "[ingress]\npublic_addresses = ['example.com']",
        "[[dns.providers]]\nkind = 'ovh'\nendpoint = 'ovh-mars'",
        "[ingress.acme]\ndirectory = 'http://acme.example.com/directory'",
        "[ingress.acme]\nemail = 'not an email'",
    ] {
        assert!(
            DaemonConfig::from_toml(invalid).is_err(),
            "accepted {invalid}"
        );
    }
}

#[test]
fn checks_locate_credential_files_without_reading_them() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("piqueld.toml");
    let check = |document: &str| {
        std::fs::write(&path, document).unwrap();
        DaemonConfig::check(&path)
    };
    // Relative names resolve against systemd's $CREDENTIALS_DIRECTORY only
    // at startup, and absent files are not read.
    let credentials = "[telemetry]\nendpoint = 'unix:///run/otel/otlp.sock'\nheaders_file = 'otlp-headers'\n\
         [ingress.private]\nauth_key_file = 'ts-apps-auth-key'\n\
         [ingress.tunnel]\nenabled = true\ncredentials_file = 'cloudflared-tunnel'\n\
         [[dns.providers]]\nkind = 'cloudflare'\napi_token_file = '/missing/token'";
    check(credentials).unwrap();
    assert!(DaemonConfig::load(&path).is_err());
    // Everything else is validated as at startup.
    for invalid in [
        "[server]\nport = 0",
        "[ingress.tunnel]\nenabled = true",
        "[telemetry]\nheaders_file = 'otlp-headers'",
        "[tailscale]\nsocket = '/run/ts.sock'\nauth_key_file = 'ts-auth-key'",
    ] {
        assert!(check(invalid).is_err(), "accepted {invalid}");
    }
    // Even after a failed check, startup reads credential files.
    std::fs::write(
        &path,
        "[[dns.providers]]\nkind = 'cloudflare'\napi_token_file = '/missing/token'",
    )
    .unwrap();
    let error = format!(
        "{:#}",
        anyhow::Error::new(DaemonConfig::load(&path).unwrap_err())
    );
    assert!(
        error.contains("could not read `api_token_file` /missing/token"),
        "{error}"
    );
    assert!(matches!(
        DaemonConfig::check(directory.path().join("missing.toml")),
        Err(ConfigError::Read(_))
    ));
}
