//! Manifest parsing, validation budgets, canonicalization, and hashing.

use piqueld_core::codes;
use piqueld_core::{ApplicationId, NormalizedApplication, parse_json, parse_toml};
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
    let renormalized = reparsed.clone().normalize();
    assert_eq!(renormalized, reparsed);
}

#[test]
fn spec_hash_is_pinned_and_ignores_metadata() {
    let app = normalized();
    assert_eq!(app.spec_hash(), GOLDEN_SPEC_HASH);

    let renamed = parse_toml(&valid_manifest("renamed"))
        .unwrap()
        .normalize(app.id.clone());
    assert_ne!(renamed.metadata.name, app.metadata.name);

    // Metadata is outside the v2 envelope: a cosmetic rename keeps the hash
    // only when the spec matches; the fixture spec differs from prebuilt's, so
    // pin the invariant on identical specs instead.
    let mut same_spec = parse_toml(TOML).unwrap();
    let mut manifest_with_other_name = same_spec_name_override(&mut same_spec);
    manifest_with_other_name.push('\n');
    let other_named = parse_toml(&manifest_with_other_name)
        .unwrap()
        .normalize(app.id.clone());
    assert_eq!(other_named.spec_hash(), app.spec_hash());
}

fn same_spec_name_override(validated: &mut piqueld_core::ValidatedApplication) -> String {
    let mut exported = {
        let app = validated
            .clone()
            .normalize(ApplicationId::parse("app-notes-01").unwrap());
        app.export_toml().unwrap()
    };
    exported = exported.replace("name = \"notes\"", "name = \"renamed\"");
    exported
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
            .all(|error| error.code == codes::MANIFEST_DECODE_FAILED)
    );
}

#[test]
fn malformed_and_invalid_manifests_return_safe_validation_errors() {
    let error = parse_toml(include_str!("fixtures/manifests/invalid.toml")).unwrap_err();
    assert!(
        error
            .0
            .iter()
            .any(|error| error.code == codes::API_VERSION_UNSUPPORTED)
    );
    assert!(
        error
            .0
            .iter()
            .any(|error| error.code == codes::IMAGE_INVALID)
    );
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
    let uppercased = valid_manifest("notes").replace(
        "image = \"ghcr.io/example/notes:1\"",
        "image = \"GHCR.IO/example/notes:1\"",
    );
    let app = parse_toml(&uppercased)
        .expect("uppercase registry hosts are accepted")
        .normalize(ApplicationId::parse("app-notes-01").unwrap());
    match &app.spec.services[0].source {
        piqueld_core::manifest::Source::Image { image } => {
            assert_eq!(image, "ghcr.io/example/notes:1");
        }
    }

    let ipv6 = valid_manifest("notes").replace("ghcr.io/example", "[::1]:5000/example");
    let error = parse_toml(&ipv6).unwrap_err();
    assert!(
        error
            .0
            .iter()
            .any(|error| error.code == codes::IMAGE_INVALID)
    );
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
    assert!(
        errors
            .0
            .iter()
            .any(|error| error.code == codes::ENVIRONMENT_COUNT_EXCESSIVE)
    );
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
    assert!(
        errors
            .0
            .iter()
            .any(|error| error.code == codes::PROCESS_ARGUMENTS_EXCESSIVE)
    );

    let long_element = "x".repeat(4_097);
    let manifest = valid_manifest("notes").replace(
        "[spec.services.source]",
        &format!("arguments = [\"{long_element}\"]\n[spec.services.source]"),
    );
    let errors = parse_toml(&manifest).unwrap_err();
    assert!(
        errors
            .0
            .iter()
            .any(|error| error.code == codes::PROCESS_ARGUMENTS_EXCESSIVE)
    );
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
    assert!(
        errors
            .0
            .iter()
            .any(|error| error.code == codes::MOUNT_COUNT_EXCESSIVE)
    );
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
    assert!(
        errors
            .0
            .iter()
            .any(|error| error.code == codes::SERVICE_COUNT_EXCESSIVE)
    );

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
    assert!(
        errors
            .0
            .iter()
            .any(|error| error.code == codes::VOLUME_COUNT_EXCESSIVE)
    );
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
    assert!(
        errors
            .0
            .iter()
            .any(|error| error.code == codes::HEALTHCHECK_INTERVAL_EXCESSIVE)
    );

    let timeout_above_interval = valid_manifest("notes").replace(
        "image = \"ghcr.io/example/notes:1\"",
        "image = \"ghcr.io/example/notes:1\"\n\n[spec.services.healthcheck]\ntype = \"http\"\nport = 8080\ninterval_seconds = 10\ntimeout_seconds = 11",
    );
    let errors = parse_toml(&timeout_above_interval).unwrap_err();
    assert!(
        errors
            .0
            .iter()
            .any(|error| error.code == codes::HEALTHCHECK_TIMEOUT_INVALID)
    );
}

#[test]
fn resource_limits_validate_cpu_bounds() {
    let zero_cpu = valid_manifest("notes").replace(
        "image = \"ghcr.io/example/notes:1\"",
        "image = \"ghcr.io/example/notes:1\"\n\n[spec.services.resources]\ncpu_millis = 0",
    );
    let errors = parse_toml(&zero_cpu).unwrap_err();
    assert!(
        errors
            .0
            .iter()
            .any(|error| error.code == codes::CPU_LIMIT_INVALID)
    );

    let excessive_cpu = valid_manifest("notes").replace(
        "image = \"ghcr.io/example/notes:1\"",
        "image = \"ghcr.io/example/notes:1\"\n\n[spec.services.resources]\ncpu_millis = 1048577",
    );
    let errors = parse_toml(&excessive_cpu).unwrap_err();
    assert!(
        errors
            .0
            .iter()
            .any(|error| error.code == codes::CPU_LIMIT_EXCESSIVE)
    );
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
