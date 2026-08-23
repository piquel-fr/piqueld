//! Focused manifest parsing and strict-schema coverage.

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
ports = [8080]
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
