//! Focused manifest parsing and strict-schema coverage.

use piqueld_core::manifest::Source;
use piqueld_core::{ApplicationId, parse_json, parse_toml};

const TOML: &str = include_str!("fixtures/manifests/prebuilt.toml");
const JSON: &str = include_str!("fixtures/manifests/prebuilt.json");

#[test]
fn image_manifest_parses_from_both_supported_formats() {
    let toml = parse_toml(TOML).expect("valid TOML manifest");
    let json = parse_json(JSON).expect("valid JSON manifest");
    assert_eq!(toml.name(), "notes");
    assert_eq!(toml.spec(), json.spec());
}

#[test]
fn normalization_is_canonical_and_round_trips() {
    let validated = parse_toml(TOML).unwrap();
    let normalized = validated.normalize(ApplicationId::parse("app-notes-01").unwrap());
    let exported = normalized.export_toml().unwrap();
    let reparsed = parse_toml(&exported)
        .unwrap()
        .normalize(normalized.id.clone());
    assert_eq!(normalized, reparsed);
    assert!(normalized.spec_hash().starts_with("sha256:"));
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
future_field = true
[spec.services.source]
type = "image"
image = "nginx:1.27"
"#,
    )
    .unwrap_err();
    assert!(
        error
            .0
            .iter()
            .all(|error| error.code == "manifest_decode_failed")
    );
}

#[test]
fn malformed_and_invalid_manifests_return_safe_validation_errors() {
    let error = parse_toml(include_str!("fixtures/manifests/invalid.toml")).unwrap_err();
    assert!(
        error
            .0
            .iter()
            .any(|error| error.code == "api_version_unsupported")
    );
    assert!(error.0.iter().any(|error| error.code == "image_invalid"));
    let error = parse_json("{\"api_version\":").unwrap_err();
    assert!(
        error
            .0
            .iter()
            .all(|error| error.code == "manifest_decode_failed")
    );
}

#[test]
fn git_source_defaults_validates_and_round_trips_without_old_fields() {
    let validated = parse_toml(
        r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "builder"
[[spec.services]]
name = "web"
[spec.services.source]
type = "git"
repository = "https://example.test/org/project.git"
"#,
    )
    .expect("Git manifest is valid")
    .normalize(ApplicationId::parse("app-builder-01").unwrap());
    assert!(matches!(
        &validated.spec.services[0].source,
        Source::Git {
            reference,
            context,
            dockerfile,
            ..
        } if reference == "main" && context == "." && dockerfile == "Dockerfile"
    ));
    let exported = validated.export_toml().unwrap();
    assert_eq!(
        parse_toml(&exported)
            .unwrap()
            .normalize(validated.id.clone()),
        validated
    );
    let invalid = parse_toml(
        r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "builder"
[[spec.services]]
name = "web"
[spec.services.source]
type = "git"
repository = "http://example.test/project.git?token=secret"
reference = "../main"
context = "../"
"#,
    )
    .unwrap_err();
    assert!(
        invalid
            .0
            .iter()
            .any(|error| error.code == "git_repository_unsupported")
    );
    assert!(
        invalid
            .0
            .iter()
            .any(|error| error.code == "git_reference_invalid")
    );
    assert!(
        invalid
            .0
            .iter()
            .any(|error| error.code == "source_path_unsafe")
    );
}

#[test]
fn routes_require_declared_ports_and_are_canonicalized() {
    let application = parse_toml(
        r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "routed"
[[spec.services]]
name = "web"
ports = [8080, 80, 8080]
[spec.services.source]
type = "image"
image = "nginx:1.27"
[[spec.routes]]
host = "WWW.Example.COM"
service = "web"
port = 8080
"#,
    )
    .expect("routed manifest is valid")
    .normalize(ApplicationId::parse("app-routed-01").unwrap());
    assert_eq!(application.spec.services[0].ports, vec![80, 8080]);
    assert_eq!(application.spec.routes[0].host, "www.example.com");

    let error = parse_toml(
        r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "routed"
[[spec.services]]
name = "web"
ports = [8080]
[spec.services.source]
type = "image"
image = "nginx:1.27"
[[spec.routes]]
host = "localhost.example"
service = "web"
port = 9090
"#,
    )
    .unwrap_err();
    assert!(
        error
            .0
            .iter()
            .any(|error| error.code == "route_port_missing")
    );
}
