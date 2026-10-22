//! Manifest parsing, validation budgets, canonicalization, and hashing.

use piqueld_core::codes;
use piqueld_core::{
    ApplicationId, NormalizedApplication, ValidationErrors, parse_json, parse_toml,
};
use proptest::prelude::*;

const TOML: &str = include_str!("fixtures/manifests/prebuilt.toml");
const JSON: &str = include_str!("fixtures/manifests/prebuilt.json");

/// The pinned v2 spec hash of the canonical `prebuilt.toml` fixture.
const GOLDEN_SPEC_HASH: &str =
    "sha256:1c681200abcdfe33443d452ffaab93268d1edbe8d18556b320fb149813edaec9";

fn normalized() -> NormalizedApplication {
    parse_toml(TOML)
        .unwrap()
        .normalize(ApplicationId::parse("app-notes-01").unwrap())
}

/// Asserts that validating `manifest` reports exactly `expected` codes and
/// paths, in order.
fn assert_errors(manifest: &str, expected: &[(&str, &str)]) {
    let errors = parse_toml(manifest).unwrap_err();
    let found = errors
        .0
        .iter()
        .map(|error| (error.code.as_str(), error.path.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(found, expected);
}

/// Whether validation reported `code`.
fn has_code(errors: &ValidationErrors, code: &str) -> bool {
    errors.0.iter().any(|error| error.code == code)
}

fn valid_manifest(name: &str) -> String {
    format!(
        r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "{name}"
[[spec.services]]
name = "web"
[spec.services.source]
type = "image"
image = "ghcr.io/example/notes:1"
"#
    )
}

#[test]
fn image_manifest_parses_from_both_supported_formats() {
    let toml = parse_toml(TOML).expect("valid TOML manifest");
    let json = parse_json(JSON).expect("valid JSON manifest");
    assert_eq!(toml.name().as_str(), "notes");
    assert_eq!(toml.spec(), json.spec());
}

#[test]
fn normalization_is_canonical_and_round_trips() {
    let validated = parse_toml(TOML).unwrap();
    let normalized = validated.normalize(ApplicationId::parse("app-notes-01").unwrap());
    let exported = normalized.export_toml().unwrap();
    let reparsed = parse_toml(&exported)
        .unwrap()
        .normalize(normalized.id().clone());
    assert_eq!(normalized, reparsed);
    let renormalized = reparsed.clone().normalize();
    assert_eq!(renormalized, reparsed);
}

#[test]
fn spec_hash_is_pinned_and_ignores_metadata() {
    let app = normalized();
    assert_eq!(app.spec_hash(), GOLDEN_SPEC_HASH);
    // Metadata is outside the v2 envelope: a cosmetic rename keeps the hash.
    let exported = app.export_toml().unwrap();
    let renamed = parse_toml(&exported.replace("name = \"notes\"", "name = \"renamed\""))
        .unwrap()
        .normalize(app.id().clone());
    assert_eq!(renamed.metadata().name.as_str(), "renamed");
    assert_eq!(renamed.spec_hash(), app.spec_hash());
}

#[test]
fn toml_and_json_inputs_produce_identical_hashes() {
    let from_toml = parse_toml(TOML).unwrap();
    let from_json = parse_json(JSON).unwrap();
    let id = ApplicationId::parse("app-notes-01").unwrap();
    assert_eq!(
        from_toml.clone().normalize(id.clone()).spec_hash(),
        from_json.normalize(id).spec_hash()
    );
}

#[test]
fn input_order_does_not_change_the_normalized_form() {
    let reordered = r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "notes"
[[spec.services]]
name = "worker"
[spec.services.source]
type = "image"
image = "ghcr.io/example/notes:1"
[[spec.services]]
name = "web"
[spec.services.source]
type = "image"
image = "ghcr.io/example/web:2"
[[spec.volumes]]
name = "zed"
[[spec.volumes]]
name = "alpha"
"#;
    let straight = r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "notes"
[[spec.services]]
name = "web"
[spec.services.source]
type = "image"
image = "ghcr.io/example/web:2"
[[spec.services]]
name = "worker"
[spec.services.source]
type = "image"
image = "ghcr.io/example/notes:1"
[[spec.volumes]]
name = "alpha"
[[spec.volumes]]
name = "zed"
"#;
    let id = ApplicationId::parse("app-notes-01").unwrap();
    let left = parse_toml(reordered).unwrap().normalize(id.clone());
    let right = parse_toml(straight).unwrap().normalize(id);
    assert_eq!(left.spec_hash(), right.spec_hash());
    assert_eq!(
        left.canonical_json().unwrap(),
        right.canonical_json().unwrap()
    );
}

#[test]
fn command_order_is_significant_but_argument_order_is_preserved_too() {
    let base = r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "notes"
[[spec.services]]
name = "web"
command = ["sh", "-c"]
arguments = ["echo", "hi"]
[spec.services.source]
type = "image"
image = "nginx:1"
"#;
    let swapped = base.replace("\"sh\", \"-c\"", "\"-c\", \"sh\"");
    let id = ApplicationId::parse("app-notes-01").unwrap();
    let left = parse_toml(base).unwrap().normalize(id.clone());
    let right = parse_toml(&swapped).unwrap().normalize(id);
    assert_ne!(left.spec_hash(), right.spec_hash());
}

/// Builds a manifest whose services declare the given startup dependencies.
fn dependency_manifest(services: &[(&str, &[&str])]) -> String {
    use std::fmt::Write;
    let mut manifest = String::from(
        "api_version = \"piqueld.dev/v1alpha1\"\nkind = \"Application\"\n[metadata]\nname = \"notes\"\n",
    );
    for (name, depends_on) in services {
        write!(
            manifest,
            "[[spec.services]]\nname = \"{name}\"\ndepends_on = {depends_on:?}\n[spec.services.source]\ntype = \"image\"\nimage = \"nginx:1.27\"\n"
        )
        .unwrap();
    }
    manifest
}

fn dependency_errors(services: &[(&str, &[&str])]) -> Vec<(String, String)> {
    parse_toml(&dependency_manifest(services))
        .unwrap_err()
        .0
        .into_iter()
        .map(|error| (error.code, error.path))
        .collect()
}

#[test]
fn service_dependencies_are_canonical_and_hashed() {
    let app = parse_toml(&dependency_manifest(&[
        ("web", &["db", "cache"]),
        ("db", &[]),
        ("cache", &[]),
    ]))
    .unwrap()
    .normalize(ApplicationId::parse("app-notes-01").unwrap());
    let web = &app.spec().services[2];
    assert_eq!(
        web.depends_on
            .iter()
            .map(piqueld_core::ServiceName::as_str)
            .collect::<Vec<_>>(),
        ["cache", "db"]
    );
    let reparsed = parse_toml(&app.export_toml().unwrap())
        .unwrap()
        .normalize(app.id().clone());
    assert_eq!(app, reparsed);
    let independent = parse_toml(&dependency_manifest(&[
        ("web", &[]),
        ("db", &[]),
        ("cache", &[]),
    ]))
    .unwrap()
    .normalize(app.id().clone());
    assert_ne!(app.spec_hash(), independent.spec_hash());
}

#[test]
fn service_dependencies_reject_unknown_duplicate_and_cyclic_names() {
    assert_eq!(
        dependency_errors(&[("web", &["db", "db", "missing"]), ("db", &[])]),
        [
            (
                codes::SERVICE_DEPENDENCY_DUPLICATE.into(),
                "spec.services[0].depends_on[1]".into()
            ),
            (
                codes::SERVICE_DEPENDENCY_MISSING.into(),
                "spec.services[0].depends_on[2]".into()
            ),
        ]
    );
    // Oversized lists are rejected before any entry is examined.
    assert_eq!(
        dependency_errors(&[("web", &["db"; 65]), ("db", &[])]),
        [(
            codes::SERVICE_DEPENDENCY_COUNT_EXCESSIVE.into(),
            "spec.services[0].depends_on".into()
        )]
    );
    assert_eq!(
        dependency_errors(&[("web", &["web"])]),
        [(
            codes::SERVICE_DEPENDENCY_CYCLE.into(),
            "spec.services[0].depends_on".into()
        )]
    );
    // Services behind a cycle cannot start either, so they are reported too.
    assert_eq!(
        dependency_errors(&[
            ("web", &["api"]),
            ("api", &["db"]),
            ("db", &["api"]),
            ("cache", &[]),
        ]),
        [0, 1, 2]
            .map(|index| (
                codes::SERVICE_DEPENDENCY_CYCLE.to_owned(),
                format!("spec.services[{index}].depends_on")
            ))
            .to_vec()
    );
}

#[test]
fn abandoned_future_manifest_fields_are_rejected() {
    let error = parse_toml(
        r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "notes"
[[spec.services]]
name = "web"
ports = [8080]
[spec.services.source]
type = "image"
image = "nginx:1.27"
"#,
    )
    .unwrap_err();
    let [error] = error.0.as_slice() else {
        panic!("expected one decode error: {error}");
    };
    assert_eq!(error.code, codes::MANIFEST_DECODE_FAILED);
    assert_eq!(error.path, "spec.services[0]");
    assert!(
        error.message.starts_with("unknown field `ports`")
            && error.message.ends_with(" at line 8 column 1"),
        "{}",
        error.message
    );
}

#[test]
fn decode_paths_keep_every_manifest_field() {
    for path in [
        "spec.services[0].files[0].mode",
        "spec.routes[0].redirect.preserve_path",
        "spec.manifest.sync.mode",
        "spec.previews.visibility",
        "spec.jobs[0].volume_access",
    ] {
        assert_eq!(piqueld_core::manifest::safe_decode_path(path), path);
    }
}

#[test]
fn malformed_and_invalid_manifests_return_safe_validation_errors() {
    let error = parse_toml(include_str!("fixtures/manifests/invalid.toml")).unwrap_err();
    assert!(has_code(&error, codes::API_VERSION_UNSUPPORTED));
    assert!(has_code(&error, codes::IMAGE_INVALID));
    let error = parse_json("{\"api_version\":").unwrap_err();
    assert!(
        error
            .0
            .iter()
            .all(|error| error.code == codes::MANIFEST_DECODE_FAILED)
    );
}

#[test]
fn validation_display_lists_every_error() {
    let error = parse_toml(include_str!("fixtures/manifests/invalid.toml")).unwrap_err();
    let display = error.to_string();
    for item in &error.0 {
        assert!(display.contains(&item.code), "{display}");
        assert!(display.contains(&item.path), "{display}");
    }
}

#[test]
fn registry_host_case_is_canonicalized_and_ipv6_is_rejected() {
    for (reference, canonical) in [
        ("GHCR.IO/example/notes:1", "ghcr.io/example/notes:1"),
        ("LOCALHOST/notes:1", "localhost/notes:1"),
    ] {
        let uppercased = valid_manifest("notes").replace(
            "image = \"ghcr.io/example/notes:1\"",
            &format!("image = \"{reference}\""),
        );
        let app = parse_toml(&uppercased)
            .expect("uppercase registry hosts are accepted")
            .normalize(ApplicationId::parse("app-notes-01").unwrap());
        match &app.spec().services[0].source {
            piqueld_core::manifest::ValidatedSource::Image { image } => {
                assert_eq!(image, canonical);
            }
            piqueld_core::manifest::ValidatedSource::Git { .. } => panic!("expected image source"),
        }
    }

    let ipv6 = valid_manifest("notes").replace("ghcr.io/example", "[::1]:5000/example");
    let error = parse_toml(&ipv6).unwrap_err();
    assert!(has_code(&error, codes::IMAGE_INVALID));
}

#[test]
fn docker_hub_shorthands_are_accepted() {
    for reference in [
        "nginx:1",
        "docker.io/library/nginx:1",
        "index.docker.io/library/nginx:1",
        "library/nginx:1",
    ] {
        let source = valid_manifest("notes").replace("ghcr.io/example/notes:1", reference);
        assert!(parse_toml(&source).is_ok(), "reference {reference}");
    }
}

#[test]
fn environment_budgets_and_key_echoes_are_enforced() {
    let oversized_value = "v".repeat(65_537);
    let manifest = format!(
        r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "notes"
[[spec.services]]
name = "web"
[spec.services.environment]
"valid_KEY" = "{oversized_value}"
[spec.services.source]
type = "image"
image = "nginx:1"
"#
    );
    let errors = parse_toml(&manifest).unwrap_err();
    let value_error = errors
        .0
        .iter()
        .find(|error| error.code == codes::ENVIRONMENT_VALUE_EXCESSIVE)
        .expect("value budget error");
    assert!(
        value_error.message.contains("'valid_KEY'"),
        "{}",
        value_error.message
    );

    let invalid_key = r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "notes"
[[spec.services]]
name = "web"
[spec.services.environment]
"9starts_digit" = "1"
[spec.services.source]
type = "image"
image = "nginx:1"
"#;
    let errors = parse_toml(invalid_key).unwrap_err();
    let key_error = errors
        .0
        .iter()
        .find(|error| error.code == codes::ENVIRONMENT_NAME_INVALID)
        .expect("key rule error");
    assert!(key_error.path.ends_with("environment.name"));
    assert!(
        key_error.message.contains("'9starts_digit'"),
        "{}",
        key_error.message
    );

    let oversized_key = format!(
        r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "notes"
[[spec.services]]
name = "web"
[spec.services.environment]
"{}" = "1"
[spec.services.source]
type = "image"
image = "nginx:1"
"#,
        "k".repeat(256)
    );
    let errors = parse_toml(&oversized_key).unwrap_err();
    let length_error = errors
        .0
        .iter()
        .find(|error| error.code == codes::ENVIRONMENT_NAME_INVALID)
        .expect("key length budget error");
    assert!(
        length_error.message.contains("at most 255 bytes"),
        "{}",
        length_error.message
    );
}

#[test]
fn ingress_proxies_environment_is_reserved() {
    let manifest = format!(
        r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "notes"
[[spec.services]]
name = "web"
[spec.services.environment]
{} = "0.0.0.0/0"
[spec.services.source]
type = "image"
image = "nginx:1"
"#,
        piqueld_core::resource::INGRESS_PROXIES_ENV
    );
    let errors = parse_toml(&manifest).unwrap_err();
    let error = errors
        .0
        .iter()
        .find(|error| error.code == codes::ENVIRONMENT_NAME_INVALID)
        .expect("reserved key error");
    assert!(error.message.contains("reserved"), "{}", error.message);
}

#[test]
fn environment_entry_count_budget_is_enforced() {
    let entries = (0..257)
        .map(|index| format!("KEY_{index} = \"1\""))
        .collect::<Vec<_>>()
        .join("\n");
    let manifest = format!(
        r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "notes"
[[spec.services]]
name = "web"
[spec.services.source]
type = "image"
image = "nginx:1"
[spec.services.environment]
{entries}
"#
    );
    let errors = parse_toml(&manifest).unwrap_err();
    assert!(has_code(&errors, codes::ENVIRONMENT_COUNT_EXCESSIVE));
}

#[test]
fn process_budgets_are_enforced() {
    let many_arguments = format!(
        r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "notes"
[[spec.services]]
name = "web"
arguments = [{}]
[spec.services.source]
type = "image"
image = "nginx:1"
"#,
        (0..129)
            .map(|index| format!("\"arg{index}\""))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let errors = parse_toml(&many_arguments).unwrap_err();
    assert!(has_code(&errors, codes::PROCESS_ARGUMENTS_EXCESSIVE));

    let long_element = "x".repeat(4_097);
    let manifest = valid_manifest("notes").replace(
        "[spec.services.source]",
        &format!("arguments = [\"{long_element}\"]\n[spec.services.source]"),
    );
    let errors = parse_toml(&manifest).unwrap_err();
    assert!(has_code(&errors, codes::PROCESS_ARGUMENTS_EXCESSIVE));
}

#[test]
fn mount_count_budget_is_enforced() {
    let volumes = (0..33)
        .map(|index| format!("[[spec.volumes]]\nname = \"vol{index}\""))
        .collect::<Vec<_>>()
        .join("\n");
    let mounts = (0..33)
        .map(|index| {
            format!("[[spec.services.mounts]]\nvolume = \"vol{index}\"\ntarget = \"/data{index}\"")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let manifest = format!(
        r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "notes"
{volumes}
[[spec.services]]
name = "web"
[spec.services.source]
type = "image"
image = "nginx:1"
{mounts}
"#
    );
    let errors = parse_toml(&manifest).unwrap_err();
    assert!(has_code(&errors, codes::MOUNT_COUNT_EXCESSIVE));
}

#[test]
fn the_health_probe_and_telemetry_paths_are_reserved() {
    for target in [
        "/run/piqueld/probe",
        "/run/piqueld/probe/inner",
        "/run/piqueld/telemetry",
        "/run/piqueld/telemetry/otlp.sock",
    ] {
        let manifest = format!(
            r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "notes"
[[spec.volumes]]
name = "data"
[[spec.services]]
name = "web"
[spec.services.source]
type = "image"
image = "nginx:1"
[[spec.services.mounts]]
volume = "data"
target = "{target}"
"#
        );
        let errors = parse_toml(&manifest).unwrap_err();
        assert!(has_code(&errors, codes::MOUNT_TARGET_UNSAFE), "{target}");
    }
}

/// One service per application receives telemetry; the flag is part of the
/// spec hash.
#[test]
fn at_most_one_service_receives_telemetry() {
    let service = |name: &str| {
        format!(
            "[[spec.services]]\nname = \"{name}\"\nreceives_telemetry = true\n[spec.services.source]\ntype = \"image\"\nimage = \"nginx:1\"\n"
        )
    };
    let header = "api_version = \"piqueld.dev/v1alpha1\"\nkind = \"Application\"\n[metadata]\nname = \"notes\"\n";
    assert_errors(
        &format!("{header}{}{}", service("a"), service("b")),
        &[(
            codes::TELEMETRY_RECEIVER_DUPLICATE,
            "spec.services[1].receives_telemetry",
        )],
    );
    let id = ApplicationId::parse("app-notes-01").unwrap();
    let receiving = parse_toml(&format!("{header}{}", service("a")))
        .unwrap()
        .normalize(id.clone());
    let plain = parse_toml(&format!(
        "{header}{}",
        service("a").replace("receives_telemetry = true\n", "")
    ))
    .unwrap()
    .normalize(id);
    assert!(receiving.spec().services[0].receives_telemetry);
    // One collector listens on the socket, and replacements stop it first.
    assert_errors(
        &format!(
            "{header}{}",
            service("a").replace("receives_telemetry", "replicas = 2\nreceives_telemetry")
        ),
        &[(
            codes::TELEMETRY_RECEIVER_REPLICAS,
            "spec.services[0].replicas",
        )],
    );
    assert_eq!(
        receiving.spec().services[0].rollout_policy().order,
        piqueld_core::manifest::RolloutOrder::StopFirst
    );
    assert_ne!(receiving.spec_hash(), plain.spec_hash());
}

#[test]
fn service_and_volume_count_budgets_are_enforced() {
    let services = (0..65)
        .map(|index| {
            format!("[[spec.services]]\nname = \"svc{index}\"\n[spec.services.source]\ntype = \"image\"\nimage = \"nginx:1\"")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let manifest = format!(
        r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "notes"
{services}
"#
    );
    let errors = parse_toml(&manifest).unwrap_err();
    assert!(has_code(&errors, codes::SERVICE_COUNT_EXCESSIVE));

    let volumes = (0..65)
        .map(|index| format!("[[spec.volumes]]\nname = \"vol{index}\""))
        .collect::<Vec<_>>()
        .join("\n");
    let manifest = format!(
        r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "notes"
[[spec.services]]
name = "web"
[spec.services.source]
type = "image"
image = "nginx:1"
{volumes}
"#
    );
    let errors = parse_toml(&manifest).unwrap_err();
    assert!(has_code(&errors, codes::VOLUME_COUNT_EXCESSIVE));
}

#[test]
fn healthcheck_rules_include_the_interval_cap_and_single_zero_error() {
    let zero_interval = valid_manifest("notes").replace(
        "image = \"ghcr.io/example/notes:1\"",
        "image = \"ghcr.io/example/notes:1\"\n\n[spec.services.healthcheck]\ntype = \"http\"\nport = 8080\ninterval_seconds = 0\ntimeout_seconds = 5",
    );
    let errors = parse_toml(&zero_interval).unwrap_err();
    let interval_errors = errors
        .0
        .iter()
        .filter(|error| error.code.starts_with("healthcheck_interval"))
        .count();
    let timeout_errors = errors
        .0
        .iter()
        .filter(|error| error.code == codes::HEALTHCHECK_TIMEOUT_INVALID)
        .count();
    assert_eq!(interval_errors, 1, "{errors:?}");
    // The dependent timeout error is suppressed when interval is zero.
    assert_eq!(timeout_errors, 0, "{errors:?}");

    let huge_interval = valid_manifest("notes").replace(
        "image = \"ghcr.io/example/notes:1\"",
        "image = \"ghcr.io/example/notes:1\"\n\n[spec.services.healthcheck]\ntype = \"http\"\nport = 8080\ninterval_seconds = 3601\ntimeout_seconds = 10",
    );
    let errors = parse_toml(&huge_interval).unwrap_err();
    assert!(has_code(&errors, codes::HEALTHCHECK_INTERVAL_EXCESSIVE));

    let timeout_above_interval = valid_manifest("notes").replace(
        "image = \"ghcr.io/example/notes:1\"",
        "image = \"ghcr.io/example/notes:1\"\n\n[spec.services.healthcheck]\ntype = \"http\"\nport = 8080\ninterval_seconds = 10\ntimeout_seconds = 11",
    );
    let errors = parse_toml(&timeout_above_interval).unwrap_err();
    assert!(has_code(&errors, codes::HEALTHCHECK_TIMEOUT_INVALID));

    let healthcheck = |settings: &str| {
        valid_manifest("notes").replace(
            "image = \"ghcr.io/example/notes:1\"",
            &format!(
                "image = \"ghcr.io/example/notes:1\"\n\n[spec.services.healthcheck]\ntype = \"http\"\nport = 8080\n{settings}"
            ),
        )
    };
    let errors = parse_toml(&healthcheck("start_period_seconds = 3601")).unwrap_err();
    assert!(has_code(&errors, codes::HEALTHCHECK_START_PERIOD_EXCESSIVE));
    for retries in [0, 101] {
        let errors = parse_toml(&healthcheck(&format!("retries = {retries}"))).unwrap_err();
        assert!(
            has_code(&errors, codes::HEALTHCHECK_RETRIES_INVALID),
            "{retries}"
        );
    }
    parse_toml(&healthcheck("start_period_seconds = 3600\nretries = 100")).unwrap();
}

#[test]
fn resource_limits_validate_cpu_and_memory_bounds_at_their_fields() {
    let manifest = |resources: &str| {
        format!(
            "{}\n[spec.services.resources]\n{resources}\n[spec.services.rollout]\nmonitor_seconds = 0",
            valid_manifest("notes")
        )
    };
    assert_errors(
        &manifest("cpu_millis = 0\nmemory_bytes = 0"),
        &[
            (
                codes::CPU_LIMIT_INVALID,
                "spec.services[0].resources.cpu_millis",
            ),
            (
                codes::MEMORY_LIMIT_INVALID,
                "spec.services[0].resources.memory_bytes",
            ),
            (
                codes::ROLLOUT_MONITOR_INVALID,
                "spec.services[0].rollout.monitor_seconds",
            ),
        ],
    );
    assert_errors(
        &manifest("cpu_millis = 1048577"),
        &[
            (
                codes::CPU_LIMIT_EXCESSIVE,
                "spec.services[0].resources.cpu_millis",
            ),
            (
                codes::ROLLOUT_MONITOR_INVALID,
                "spec.services[0].rollout.monitor_seconds",
            ),
        ],
    );
    assert_errors(
        &manifest(""),
        &[
            (codes::RESOURCE_LIMITS_EMPTY, "spec.services[0].resources"),
            (
                codes::ROLLOUT_MONITOR_INVALID,
                "spec.services[0].rollout.monitor_seconds",
            ),
        ],
    );
}

/// Every per-item validator reports at its own field, in path order.
#[test]
fn validators_report_each_error_at_its_field_in_path_order() {
    let manifest = r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "notes"
[[spec.volumes]]
name = "Bad"
[[spec.routes]]
hostname = "old.example.com"
redirect = { to = "ftp://example.com", status = 200 }
[[spec.secrets]]
name = "key"
generate = { type = "random", bytes = 8 }
[[spec.jobs]]
name = "migrate"
service = "api"
command = ["notes"]
run = "before-rollout"
[[spec.services]]
name = "web"
replicas = 0
command = [""]
depends_on = ["missing"]
secrets = [{ name = "key", target = "/etc/key" }]
files = [{ target = "/run/secrets/x", content = "" }]
mounts = [{ volume = "missing", target = "/data" }]
[spec.services.source]
type = "image"
image = "Nginx"
[spec.services.environment]
"9x" = "1"
[spec.services.healthcheck]
type = "http"
port = 0
interval_seconds = 0
"#;
    assert_errors(
        manifest,
        &[
            (codes::JOB_SERVICE_MISSING, "spec.jobs[0].service"),
            (
                codes::ROUTE_REDIRECT_STATUS_INVALID,
                "spec.routes[0].redirect.status",
            ),
            (codes::ROUTE_REDIRECT_INVALID, "spec.routes[0].redirect.to"),
            (
                codes::SECRET_GENERATOR_INVALID,
                "spec.secrets[0].generate.bytes",
            ),
            (
                codes::PROCESS_COMMAND_INVALID,
                "spec.services[0].command[0]",
            ),
            (
                codes::SERVICE_DEPENDENCY_MISSING,
                "spec.services[0].depends_on[0]",
            ),
            (
                codes::ENVIRONMENT_NAME_INVALID,
                "spec.services[0].environment.name",
            ),
            (codes::FILE_EMPTY, "spec.services[0].files[0].content"),
            (
                codes::FILE_TARGET_INVALID,
                "spec.services[0].files[0].target",
            ),
            (
                codes::HEALTHCHECK_INTERVAL_INVALID,
                "spec.services[0].healthcheck.interval_seconds",
            ),
            (codes::PORT_INVALID, "spec.services[0].healthcheck.port"),
            (
                codes::MOUNT_VOLUME_MISSING,
                "spec.services[0].mounts[0].volume",
            ),
            (codes::REPLICAS_OUT_OF_RANGE, "spec.services[0].replicas"),
            (codes::SECRET_TARGET_INVALID, "spec.services[0].secrets[0]"),
            (codes::IMAGE_INVALID, "spec.services[0].source.image"),
            (codes::NAME_INVALID, "spec.volumes[0].name"),
        ],
    );
}

/// The fixture with `settings` in its service's rollout block.
fn rollout_manifest(settings: &str) -> String {
    format!("{TOML}\n[spec.services.rollout]\n{settings}\n")
}

#[test]
fn rollout_settings_are_typed_bounded_and_backwards_compatible() {
    use piqueld_core::manifest::{RolloutOrder, ValidatedRollout as Rollout};
    let id = ApplicationId::parse("app-notes-01").unwrap();

    // An empty block is the same specification as none.
    let empty = parse_toml(&rollout_manifest(""))
        .unwrap()
        .normalize(id.clone());
    assert_eq!(empty.spec_hash(), GOLDEN_SPEC_HASH);
    assert!(!empty.export_toml().unwrap().contains("rollout"));

    let explicit = parse_toml(&rollout_manifest(
        "order = \"start-first\"\nmonitor_seconds = 3600",
    ))
    .unwrap()
    .normalize(id.clone());
    assert_eq!(
        explicit.spec().services[0].rollout,
        Rollout {
            order: Some(RolloutOrder::StartFirst),
            monitor_seconds: Some(3600),
        }
    );
    assert_ne!(explicit.spec_hash(), GOLDEN_SPEC_HASH);
    let reparsed = parse_toml(&explicit.export_toml().unwrap())
        .unwrap()
        .normalize(id);
    assert_eq!(explicit, reparsed);

    for monitor in [0, Rollout::MAX_MONITOR_SECONDS + 1] {
        let errors = parse_toml(&rollout_manifest(&format!("monitor_seconds = {monitor}")))
            .unwrap_err()
            .0;
        assert_eq!(
            errors
                .iter()
                .map(|error| (error.code.as_str(), error.path.as_str()))
                .collect::<Vec<_>>(),
            [(
                codes::ROLLOUT_MONITOR_INVALID,
                "spec.services[0].rollout.monitor_seconds"
            )]
        );
    }
    let unknown = parse_toml(&rollout_manifest("order = \"rolling\"")).unwrap_err();
    assert_eq!(unknown.0[0].code, codes::MANIFEST_DECODE_FAILED);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn parsing_arbitrary_text_never_panics(input in "\\PC*") {
        let _ = parse_toml(&input);
        let _ = parse_json(&input);
    }

    #[test]
    fn normalization_is_idempotent(name in "[a-z][a-z0-9-]{0,62}") {
        prop_assume!(!name.ends_with('-'));
        let manifest = valid_manifest(&name);
        if let Ok(validated) = parse_toml(&manifest) {
            let id = ApplicationId::parse("app-notes-01").unwrap();
            let once = validated.normalize(id.clone()).normalize();
            let twice = once.clone().normalize();
            prop_assert_eq!(once, twice);
        }
    }

    #[test]
    fn hashes_are_stable_across_renormalization(name in "[a-z][a-z0-9-]{0,62}") {
        prop_assume!(!name.ends_with('-'));
        let manifest = valid_manifest(&name);
        if let Ok(validated) = parse_toml(&manifest) {
            let id = ApplicationId::parse("app-notes-01").unwrap();
            let app = validated.normalize(id);
            prop_assert_eq!(app.spec_hash(), app.clone().normalize().spec_hash());
        }
    }
}

#[test]
fn normalized_deserialization_revalidates_and_canonicalizes_configuration() {
    let original = parse_toml(&valid_manifest("notes"))
        .unwrap()
        .normalize(ApplicationId::parse("app-domain-01").unwrap());
    let mut wire = serde_json::to_value(&original).unwrap();
    let mut later = wire["spec"]["services"][0].clone();
    later["name"] = "z-last".into();
    let mut earlier = later.clone();
    earlier["name"] = "a-first".into();
    wire["spec"]["services"] = serde_json::json!([later, earlier]);
    let decoded: piqueld_core::NormalizedApplication =
        serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(decoded.spec().services[0].name.as_str(), "a-first");
    assert_eq!(decoded.spec().services[1].name.as_str(), "z-last");
    assert_eq!(decoded.id(), original.id());
    let expected = decoded
        .to_manifest()
        .validate()
        .unwrap()
        .normalize(original.id().clone());
    assert_eq!(decoded.spec_hash(), expected.spec_hash());

    for pointer in ["/metadata/name", "/spec/services/0/name"] {
        let mut malformed = wire.clone();
        *malformed.pointer_mut(pointer).unwrap() = "INVALID".into();
        assert!(serde_json::from_value::<piqueld_core::NormalizedApplication>(malformed).is_err());
    }
    wire["spec"]["services"][0]["mounts"] = serde_json::json!([
        {"volume": "undeclared", "target": "/data", "read_only": false}
    ]);
    let error = serde_json::from_value::<piqueld_core::NormalizedApplication>(wire).unwrap_err();
    assert!(error.to_string().contains("mount"), "{error}");
}

#[test]
fn image_repository_rejects_invalid_internal_bytes() {
    for image in ["a!a", "aAa", "aéa", "a💥a", "example/a+a", "example/a___a"] {
        assert!(
            piqueld_core::ImageReference::parse(image).is_err(),
            "{image}"
        );
    }
    for image in ["a.a", "a_a", "a__a", "a---a", "example/a-b:latest"] {
        assert!(
            piqueld_core::ImageReference::parse(image).is_ok(),
            "{image}"
        );
    }
}

proptest! {
    #[test]
    fn image_parser_terminates_for_arbitrary_repository_contents(input in "\\PC{0,128}") {
        let _ = piqueld_core::ImageReference::parse(format!("a{input}a"));
    }
}

/// The schema rejects what serde rejects: unknown fields of tagged variants
/// and integers beyond their Rust type.
#[test]
fn manifest_json_schema_matches_serde_strictness() {
    use piqueld_core::manifest::{Build, HealthCheck, SecretGenerator, Source};
    use piqueld_core::sync::RepositorySync;
    use serde::de::DeserializeOwned;
    use serde_json::{Value, json};

    /// Whether serde decodes a value as one type.
    type Decodes = fn(&Value) -> bool;

    fn decodes<T: DeserializeOwned>(value: &Value) -> bool {
        serde_json::from_value::<T>(value.clone()).is_ok()
    }
    let schema = piqueld_core::manifest::ApplicationManifest::json_schema();
    let definitions = &schema["definitions"];
    // Each value adds `extra` to a valid variant: the variant's schema admits
    // it exactly when serde does.
    let cases: [(&str, &str, Value, Decodes); 7] = [
        (
            "RepositorySync",
            "mode",
            json!({"mode": "off", "extra": 300}),
            decodes::<RepositorySync>,
        ),
        (
            "RepositorySync",
            "mode",
            json!({"mode": "webhook", "extra": 300}),
            decodes::<RepositorySync>,
        ),
        (
            "RepositorySync",
            "mode",
            json!({"mode": "poll", "extra": 300}),
            decodes::<RepositorySync>,
        ),
        (
            "Source",
            "type",
            json!({"type": "image", "image": "nginx:1", "extra": 1}),
            decodes::<Source>,
        ),
        (
            "Build",
            "type",
            json!({"type": "docker", "dockerfile": "Dockerfile", "extra": 1}),
            decodes::<Build>,
        ),
        (
            "HealthCheck",
            "type",
            json!({"type": "command", "command": ["true"], "extra": 1}),
            decodes::<HealthCheck>,
        ),
        (
            "SecretGenerator",
            "type",
            json!({"type": "rsa", "bits": 3072, "extra": 1}),
            decodes::<SecretGenerator>,
        ),
    ];
    for (name, tag, value, decodes) in cases {
        let variant = definitions[name]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|variant| variant["properties"][tag]["enum"][0] == value[tag])
            .unwrap();
        let admitted = variant["additionalProperties"] != false;
        assert_eq!(admitted, decodes(&value), "{value}");
    }
    assert_eq!(definitions["Typed_u16"]["oneOf"][0]["maximum"], 65_535);
    assert_eq!(definitions["Typed_u32"]["oneOf"][0]["maximum"], u32::MAX);
    assert_eq!(definitions["Typed_u64"]["oneOf"][0]["maximum"], u64::MAX);
    assert_eq!(
        definitions["Job"]["properties"]["timeout_seconds"]["maximum"],
        u32::MAX
    );
    assert_eq!(
        definitions["SecretGenerator"]["oneOf"][0]["properties"]["bytes"]["maximum"],
        65_535
    );
}

#[test]
fn manifest_json_schema_is_self_contained() {
    fn references(value: &serde_json::Value, found: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(object) => {
                if let Some(reference) = object.get("$ref").and_then(|r| r.as_str()) {
                    found.push(reference.to_owned());
                }
                for value in object.values() {
                    references(value, found);
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    references(value, found);
                }
            }
            _ => {}
        }
    }

    let schema = piqueld_core::manifest::ApplicationManifest::json_schema();
    let mut found = Vec::new();
    references(&schema, &mut found);
    assert_ne!(found, [] as [std::string::String; 0]);
    for reference in found {
        let name = reference
            .strip_prefix("#/definitions/")
            .unwrap_or_else(|| panic!("{reference} must point into the schema's definitions"));
        assert!(
            schema["definitions"].get(name).is_some(),
            "{reference} has no definition"
        );
    }
}

#[test]
fn generated_secrets_default_round_trip_and_validate() {
    use piqueld_core::manifest::{SecretEncoding, SecretGenerator};
    let declared = |secrets: &str| format!("{}{secrets}", valid_manifest("notes"));
    let app = parse_toml(&declared(
        r#"
[[spec.secrets]]
name = "signing-key"
generate = { type = "rsa", bits = 3072 }
[[spec.secrets]]
name = "database-password"
generate = { type = "random", bytes = 32 }
"#,
    ))
    .unwrap()
    .normalize(ApplicationId::parse("app-notes-01").unwrap());
    let secrets = &app.spec().secrets;
    assert_eq!(secrets[0].name, "database-password");
    assert_eq!(
        secrets[0].generate,
        SecretGenerator::Random {
            bytes: 32,
            encoding: SecretEncoding::Hex
        }
    );
    let reparsed = parse_toml(&app.export_toml().unwrap())
        .unwrap()
        .normalize(app.id().clone());
    assert_eq!(app, reparsed);

    let error = parse_toml(&declared(
        r#"
[[spec.secrets]]
name = "key"
generate = { type = "rsa", bits = 1024 }
[[spec.secrets]]
name = "key"
generate = { type = "random", bytes = 8, encoding = "base64url" }
"#,
    ))
    .unwrap_err();
    let found = error
        .0
        .iter()
        .map(|error| (error.code.as_str(), error.path.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        found,
        [
            ("secret_generator_invalid", "spec.secrets[0].generate.bits"),
            ("secret_generator_invalid", "spec.secrets[1].generate.bytes"),
            ("secret_name_duplicate", "spec.secrets[1].name"),
        ]
    );
}

#[test]
fn jobs_keep_declared_order_and_reference_declared_services() {
    let jobs = r#"
[[spec.jobs]]
name = "migrate"
service = "web"
command = ["notes", "migrate"]
run = "before-rollout"
[[spec.jobs]]
name = "seed"
service = "web"
command = ["notes", "seed"]
run = "before-rollout"
timeout_seconds = 60
"#;
    let app = parse_toml(&(valid_manifest("notes") + jobs))
        .unwrap()
        .normalize(ApplicationId::parse("app-notes-01").unwrap());
    let names = app
        .spec()
        .jobs
        .iter()
        .map(|job| (job.name.as_str(), job.timeout_seconds))
        .collect::<Vec<_>>();
    assert_eq!(names, [("migrate", 300), ("seed", 60)]);
    let reparsed = parse_toml(&app.export_toml().unwrap())
        .unwrap()
        .normalize(app.id().clone());
    assert_eq!(reparsed, app);
    let without_jobs = parse_toml(&valid_manifest("notes"))
        .unwrap()
        .normalize(app.id().clone());
    assert_ne!(app.spec_hash(), without_jobs.spec_hash());

    let invalid = r#"
[[spec.jobs]]
name = "migrate"
service = "api"
command = [""]
run = "before-rollout"
timeout_seconds = 0
[[spec.jobs]]
name = "migrate"
service = "web"
command = ["notes"]
run = "before-rollout"
"#;
    let error = parse_toml(&(valid_manifest("notes") + invalid)).unwrap_err();
    let found = error
        .0
        .iter()
        .map(|error| (error.code.as_str(), error.path.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        found,
        [
            (codes::PROCESS_COMMAND_INVALID, "spec.jobs[0].command"),
            (codes::JOB_SERVICE_MISSING, "spec.jobs[0].service"),
            (codes::JOB_TIMEOUT_INVALID, "spec.jobs[0].timeout_seconds"),
            (codes::JOB_NAME_DUPLICATE, "spec.jobs[1].name"),
        ]
    );
}

#[test]
fn jobs_add_environment_and_secrets_their_service_does_not_set() {
    let job = r#"
[spec.services.environment]
RUST_LOG = "info"
[[spec.services.secrets]]
name = "app-key"
target = "/run/secrets/app-key"
[[spec.jobs]]
name = "migrate"
service = "web"
command = ["notes", "migrate"]
run = "before-rollout"
[spec.jobs.environment]
OWNER_PASSWORD_FILE = "/run/secrets/owner"
[[spec.jobs.secrets]]
name = "owner-password"
target = "/run/secrets/owner"
"#;
    let app = parse_toml(&(valid_manifest("notes") + job))
        .unwrap()
        .normalize(ApplicationId::parse("app-notes-01").unwrap());
    assert_eq!(
        app.spec().mounted_secret_names(),
        ["app-key", "owner-password"].into()
    );
    let reparsed = parse_toml(&app.export_toml().unwrap())
        .unwrap()
        .normalize(app.id().clone());
    assert_eq!(reparsed, app);

    // A job can't override what its service sets.
    let mut conflicting = app.to_manifest();
    let job = &mut conflicting.spec.jobs[0];
    job.environment.insert("RUST_LOG".into(), "debug".into());
    job.secrets.push(piqueld_core::manifest::SecretMount {
        name: "other".into(),
        target: "/run/secrets/app-key".into(),
    });
    let found = conflicting
        .validate()
        .unwrap_err()
        .0
        .into_iter()
        .map(|error| (error.code, error.path))
        .collect::<Vec<_>>();
    assert_eq!(
        found,
        [
            (
                codes::JOB_SETTING_CONFLICT.to_owned(),
                "spec.jobs[0].environment.name".to_owned()
            ),
            (
                codes::JOB_SETTING_CONFLICT.to_owned(),
                "spec.jobs[0].secrets[1].target".to_owned()
            ),
        ]
    );
}

#[test]
fn dependency_jobs_must_precede_jobs_that_start_their_services() {
    let mut manifest = parse_toml(&format!(
        "{}\n[[spec.services]]\nname = \"db\"\ndepends_on = [\"storage\"]\n\
         [spec.services.source]\ntype = \"image\"\nimage = \"postgres:17\"\n\
         [[spec.services]]\nname = \"storage\"\n[spec.services.source]\n\
         type = \"image\"\nimage = \"storage:1\"\n",
        valid_manifest("notes")
    ))
    .unwrap()
    .normalize(ApplicationId::parse("app-notes-01").unwrap())
    .to_manifest();
    manifest
        .spec
        .services
        .iter_mut()
        .find(|service| service.name == "web")
        .unwrap()
        .depends_on = vec!["db".into()];
    manifest.spec.jobs = ["storage", "db", "web"]
        .into_iter()
        .map(|service| piqueld_core::manifest::Job {
            name: format!("initialize-{service}"),
            service: service.into(),
            command: vec!["initialize".into()],
            environment: std::collections::BTreeMap::new(),
            secrets: Vec::new(),
            run: piqueld_core::manifest::JobRun::BeforeRollout,
            timeout_seconds: 300,
            volume_access: piqueld_core::manifest::JobVolumeAccess::ReadOnly,
        })
        .collect();
    manifest.clone().validate().unwrap();
    for dependency in [0, 1] {
        let mut invalid = manifest.clone();
        invalid.spec.jobs.swap(dependency, 2);
        let errors = invalid.validate().unwrap_err();
        assert!(
            errors
                .0
                .iter()
                .any(|error| error.code == codes::JOB_DEPENDENCY_ORDER_INVALID
                    && error.path == format!("spec.jobs[{dependency}].service"))
        );
    }
}

#[test]
fn exclusive_jobs_need_a_writable_volume_their_dependencies_do_not_mount() {
    let manifest = parse_toml(&format!(
        "{}\n[[spec.services]]\nname = \"db\"\n[spec.services.source]\n\
         type = \"image\"\nimage = \"postgres:17\"\n\
         [[spec.volumes]]\nname = \"data\"\n\
         [[spec.jobs]]\nname = \"migrate\"\nservice = \"web\"\n\
         command = [\"notes\", \"migrate\"]\nrun = \"before-rollout\"\n",
        valid_manifest("notes")
    ))
    .unwrap()
    .normalize(ApplicationId::parse("app-notes-01").unwrap())
    .to_manifest();
    let mount = |read_only| piqueld_core::manifest::Mount {
        volume: "data".into(),
        target: "/data".into(),
        read_only,
    };
    // Mount flags of web and db, and the number of errors expected.
    for (web, db, errors) in [
        (Some(false), None, 0),
        (Some(true), None, 1),
        (None, None, 1),
        (Some(false), Some(true), 1),
    ] {
        let mut input = manifest.clone();
        input.spec.jobs[0].volume_access = piqueld_core::manifest::JobVolumeAccess::Exclusive;
        for service in &mut input.spec.services {
            let (mounted, depends_on) = if service.name == "web" {
                (web, vec!["db".into()])
            } else {
                (db, Vec::new())
            };
            service.mounts = mounted.map(mount).into_iter().collect();
            service.depends_on = depends_on;
        }
        let found = input
            .validate()
            .err()
            .map(|errors| errors.0)
            .unwrap_or_default()
            .into_iter()
            .map(|error| (error.code, error.path))
            .collect::<Vec<_>>();
        assert_eq!(
            found,
            vec![
                (
                    codes::JOB_VOLUME_ACCESS_INVALID.to_owned(),
                    "spec.jobs[0].volume_access".to_owned()
                );
                errors
            ]
        );
    }
}

#[test]
fn links_validate_both_sides_and_round_trip() {
    let links = r#"
[[spec.exports]]
service = "web"
applications = ["billing", "auth"]
previews = true
[[spec.imports]]
application = "auth"
service = "api"
environment = "staging"
[[spec.imports]]
application = "auth"
service = "events"
"#;
    let app = parse_toml(&format!("{}{links}", valid_manifest("notes")))
        .unwrap()
        .normalize(ApplicationId::parse("app-notes-01").unwrap());
    assert_eq!(app.spec().exports[0].service.as_str(), "web");
    assert_eq!(app.spec().exports[0].applications.len(), 2);
    assert_eq!(app.spec().imports[0].alias(), "api.auth");
    assert_eq!(
        app.spec().imports[0]
            .environment
            .as_ref()
            .map(piqueld_core::EnvironmentName::as_str),
        Some("staging")
    );
    let reparsed = parse_toml(&app.export_toml().unwrap())
        .unwrap()
        .normalize(app.id().clone());
    assert_eq!(reparsed, app);

    let invalid = r#"
[[spec.exports]]
service = "missing"
applications = []
[[spec.exports]]
service = "web"
applications = ["notes", "auth", "auth"]
[[spec.imports]]
application = "notes"
service = "web"
[[spec.imports]]
application = "auth"
service = "api"
environment = "Staging"
[[spec.imports]]
application = "auth"
service = "api"
"#;
    let errors = parse_toml(&format!("{}{invalid}", valid_manifest("notes")))
        .unwrap_err()
        .0
        .into_iter()
        .map(|error| (error.code, error.path))
        .collect::<Vec<_>>();
    let expected = [
        (codes::EXPORT_SERVICE_MISSING, "spec.exports[0].service"),
        (
            codes::EXPORT_APPLICATIONS_EMPTY,
            "spec.exports[0].applications",
        ),
        (codes::EXPORT_DUPLICATE, "spec.exports[1].applications[2]"),
        (codes::NAME_INVALID, "spec.imports[1].environment"),
        (codes::IMPORT_DUPLICATE, "spec.imports[2].service"),
    ];
    for (code, path) in expected {
        assert!(
            errors.contains(&(code.to_owned(), path.to_owned())),
            "missing {code} at {path} in {errors:?}"
        );
    }
    assert_eq!(errors.len(), expected.len(), "{errors:?}");
}
