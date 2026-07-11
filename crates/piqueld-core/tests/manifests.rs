//! Manifest contract, fixture, property, and schema snapshot tests.

use piqueld_core::manifest::{application_manifest_schema, normalized_application_schema};
use piqueld_core::{ApplicationId, parse_json, parse_toml};
use proptest::prelude::*;
use sha2::{Digest, Sha256};

const ID: &str = "01jz8r7b4w-test";
fn id() -> ApplicationId {
    ApplicationId::parse(ID).unwrap()
}

#[test]
fn toml_and_json_golden_normalize_identically() {
    let toml = include_str!("fixtures/manifests/prebuilt.toml");
    let json = include_str!("fixtures/manifests/prebuilt.json");
    assert_eq!(
        parse_toml(toml).unwrap().normalize(id()),
        parse_json(json).unwrap().normalize(id())
    );
}

#[test]
fn defaults_and_export_round_trip() {
    let app = parse_toml(include_str!("fixtures/manifests/defaults.toml"))
        .unwrap()
        .normalize(id());
    assert_eq!(app.spec.services[0].replicas, 1);
    let exported = app.export_toml().unwrap();
    assert_eq!(app, parse_toml(&exported).unwrap().normalize(id()));
    assert!(!exported.to_ascii_lowercase().contains("plaintext"));
}

#[test]
fn multi_service_git_fixture_and_secret_hook() {
    let validated = parse_toml(include_str!("fixtures/manifests/git-multi.toml")).unwrap();
    assert_eq!(
        validated
            .logical_secret_references()
            .into_iter()
            .collect::<Vec<_>>(),
        ["database-url"]
    );
    assert!(
        validated
            .validate_secret_references(|name| name == "database-url")
            .is_ok()
    );
    assert_eq!(
        validated
            .validate_secret_references(|_| false)
            .unwrap_err()
            .0[0]
            .code,
        "logical_secret_missing"
    );
    let app = validated.normalize(id());
    assert_eq!(app.spec.services[0].name, "web");
    assert_eq!(app.spec.services[1].name, "worker");
}

#[test]
fn unordered_input_has_the_same_hash_and_commands_remain_ordered() {
    let first = parse_toml(include_str!("fixtures/manifests/git-multi.toml"))
        .unwrap()
        .normalize(id());
    let mut value: serde_json::Value =
        serde_json::from_str(&first.canonical_json().unwrap()).unwrap();
    value["spec"]["services"].as_array_mut().unwrap().reverse();
    value.as_object_mut().unwrap().remove("id");
    let second = parse_json(&serde_json::to_string(&value).unwrap())
        .unwrap()
        .normalize(id());
    assert_eq!(first.spec_hash(), second.spec_hash());
    assert_eq!(second.spec.services[1].arguments, ["--queue", "default"]);
}

#[test]
fn representative_invalid_fixture_has_field_errors() {
    let errors = parse_toml(include_str!("fixtures/manifests/invalid.toml")).unwrap_err();
    for code in [
        "api_version_unsupported",
        "name_invalid",
        "replicas_out_of_range",
        "image_invalid",
        "route_host_invalid",
        "route_service_missing",
        "port_invalid",
    ] {
        assert!(
            errors.0.iter().any(|error| error.code == code),
            "missing {code}"
        );
    }
    assert!(errors.0.iter().all(|error| !error.path.is_empty()));
}

#[test]
fn strict_unknown_fields_and_unsupported_sources_are_rejected() {
    let base = include_str!("fixtures/manifests/prebuilt.toml");
    assert_eq!(
        parse_toml(&format!("{base}\nunknown = true"))
            .unwrap_err()
            .0[0]
            .code,
        "manifest_decode_failed"
    );
    assert!(parse_json(r#"{"api_version":"piqueld.dev/v1alpha1","kind":"Application","metadata":{"name":"a"},"spec":{"services":[{"name":"web","source":{"type":"ssh","repository":"x"}}]}}"#).is_err());
}

#[test]
fn duplicate_and_reference_rules_are_exhaustive() {
    let input = r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "test"
[[spec.services]]
name = "web"
[spec.services.source]
type = "image"
image = "alpine"
[[spec.services.mounts]]
volume = "missing"
target = "/data"
[[spec.services.mounts]]
volume = "missing"
target = "/data"
[[spec.services.secrets]]
source = "one"
target = "token"
[[spec.services.secrets]]
source = "two"
target = "token"
[[spec.services]]
name = "web"
[spec.services.source]
type = "image"
image = "alpine"
[[spec.volumes]]
name = "data"
[[spec.volumes]]
name = "data"
[[spec.routes]]
host = "same.example.com"
service = "web"
port = 80
[[spec.routes]]
host = "SAME.example.com"
service = "web"
port = 81
"#;
    let errors = parse_toml(input).unwrap_err();
    for code in [
        "service_name_duplicate",
        "volume_name_duplicate",
        "mount_volume_missing",
        "mount_target_duplicate",
        "secret_target_duplicate",
        "public_route_conflict",
    ] {
        assert!(
            errors.0.iter().any(|error| error.code == code),
            "missing {code}"
        );
    }
}

#[test]
fn schema_snapshots_are_stable() {
    let request = serde_json::to_string_pretty(&application_manifest_schema()).unwrap();
    let response = serde_json::to_string_pretty(&normalized_application_schema()).unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(request)),
        include_str!("snapshots/application-manifest.schema.sha256").trim()
    );
    assert_eq!(
        format!("{:x}", Sha256::digest(response)),
        include_str!("snapshots/normalized-application.schema.sha256").trim()
    );
}

proptest! {
    #[test]
    fn parsing_arbitrary_input_never_panics(input in any::<String>()) {
        let _ = parse_toml(&input);
        let _ = parse_json(&input);
    }

    #[test]
    fn normalization_and_hash_are_stable(mut order in prop::collection::vec(1u16..65535, 0..30)) {
        order.push(3000);
        let mut json: serde_json::Value = serde_json::from_str(include_str!("fixtures/manifests/prebuilt.json")).unwrap();
        json["spec"]["services"][0]["ports"] = serde_json::json!(order);
        let app = parse_json(&json.to_string()).unwrap().normalize(id());
        prop_assert_eq!(app.clone().normalize(), app.clone());
        prop_assert_eq!(app.spec_hash(), app.clone().spec_hash());
    }
}
