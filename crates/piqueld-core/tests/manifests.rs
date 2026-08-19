//! Manifest contract, fixture, and property tests.

use piqueld_core::{
    ApplicationId, ResourceKind, docker_resource_name, parse_json, parse_toml, router_name,
};
use proptest::prelude::*;

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
    let piqueld_core::manifest::Source::Git {
        reference,
        context,
        dockerfile,
        ..
    } = &app.spec.services[0].source
    else {
        panic!("defaults fixture must use Git")
    };
    assert_eq!(reference, "main");
    assert_eq!(context, ".");
    assert_eq!(dockerfile, "Dockerfile");
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

    let field_error = parse_json(
        r#"{"api_version":"piqueld.dev/v1alpha1","kind":"Application","metadata":{"name":"a"},"spec":{"services":[{"name":"web","source":{"type":"image","image":"alpine"},"do_not_echo_sensitive_token":true}]}}"#,
    )
    .unwrap_err();
    assert_eq!(field_error.0[0].path, "spec.services[0]");
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
fn rejects_unsafe_runtime_values_paths_modes_and_target_collisions() {
    let input = r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "test"
[[spec.services]]
name = "web"
command = ["web\u0000server"]
arguments = ["--config\u0000bad"]
[spec.services.source]
type = "git"
repository = "https://"
context = "src//nested"
dockerfile = "../Dockerfile"
[[spec.services.mounts]]
volume = "data"
target = "/run//shared"
[[spec.services.secrets]]
source = "token"
target = "/run//shared"
mode = "0420"
[[spec.volumes]]
name = "data"
"#;
    let errors = parse_toml(input).unwrap_err();
    for code in [
        "git_repository_unsupported",
        "source_path_unsafe",
        "mount_target_unsafe",
        "secret_mode_invalid",
        "target_collision",
        "process_argument_invalid",
    ] {
        assert!(
            errors.0.iter().any(|error| error.code == code),
            "missing {code}: {errors:?}"
        );
    }
}

#[test]
fn git_repository_credentials_are_not_accepted_in_manifests() {
    let input = r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "test"
[[spec.services]]
name = "web"
[spec.services.source]
type = "git"
repository = "https://token@example.com/private.git"
"#;
    let errors = parse_toml(input).unwrap_err();
    assert!(
        errors
            .0
            .iter()
            .any(|error| error.code == "git_repository_unsupported")
    );
    assert!(
        errors
            .0
            .iter()
            .all(|error| !error.message.contains("token"))
    );
}

#[test]
fn image_and_git_sources_reject_ambiguous_or_sensitive_references() {
    for image in [
        "!",
        "https://registry.example.com/team/image:latest",
        "registry.example.com/Team/image:latest",
        "token@example.com/private",
        "alpine:",
        "alpine@sha256:short",
    ] {
        let input = format!(
            r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "test"
[[spec.services]]
name = "web"
[spec.services.source]
type = "image"
image = "{image}"
"#
        );
        assert!(
            parse_toml(&input)
                .unwrap_err()
                .0
                .iter()
                .any(|error| error.code == "image_invalid"),
            "image {image:?} should be rejected"
        );
    }
    for image in [
        "alpine",
        "alpine:3.20",
        "registry.example.com:5000/team/image:Release-1",
        "team/my__image@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    ] {
        let input = format!(
            r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "test"
[[spec.services]]
name = "web"
[spec.services.source]
type = "image"
image = "{image}"
"#
        );
        assert!(
            parse_toml(&input).is_ok(),
            "image {image:?} should be accepted"
        );
    }

    for repository in [
        "https://example.com/private.git?access_token=sensitive",
        "https://example.com/private.git#sensitive",
        "ftp://example.com/private.git",
    ] {
        let input = format!(
            r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "test"
[[spec.services]]
name = "web"
[spec.services.source]
type = "git"
repository = "{repository}"
"#
        );
        let errors = parse_toml(&input).unwrap_err();
        assert!(
            errors
                .0
                .iter()
                .any(|error| error.code == "git_repository_unsupported")
        );
        assert!(
            errors
                .0
                .iter()
                .all(|error| !error.message.contains("sensitive"))
        );
    }
}

#[test]
fn unsafe_git_references_are_rejected() {
    for reference in ["../main", "refs//heads/main", "main.lock", "main~1", "@{"] {
        let input = format!(
            r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "test"
[[spec.services]]
name = "web"
[spec.services.source]
type = "git"
repository = "https://example.com/repository.git"
reference = "{reference}"
"#
        );
        assert!(
            parse_toml(&input)
                .unwrap_err()
                .0
                .iter()
                .any(|error| error.code == "git_reference_invalid"),
            "Git reference {reference:?} should be rejected"
        );
    }
}

#[test]
fn invalid_absolute_secret_targets_have_secret_specific_errors() {
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
[[spec.services.secrets]]
source = "token"
target = "/run//token"
"#;
    let errors = parse_toml(input).unwrap_err();
    assert!(
        errors
            .0
            .iter()
            .any(|error| error.code == "secret_target_unsafe")
    );
    assert!(
        errors
            .0
            .iter()
            .all(|error| error.code != "mount_target_unsafe")
    );
}

#[test]
fn effective_secret_paths_cannot_collide_with_mounts_or_each_other() {
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
volume = "data"
target = "/run/secrets/token"
[[spec.services.secrets]]
source = "one"
target = "token"
[[spec.services.secrets]]
source = "two"
target = "/run/secrets/token"
[[spec.volumes]]
name = "data"
"#;
    let errors = parse_toml(input).unwrap_err();
    assert!(
        errors
            .0
            .iter()
            .any(|error| error.code == "secret_target_duplicate")
    );
    assert_eq!(
        errors
            .0
            .iter()
            .filter(|error| error.code == "target_collision")
            .count(),
        2
    );
}

#[test]
fn applications_require_a_service_and_health_commands_require_an_executable() {
    let no_services = r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "empty"
[spec]
"#;
    assert!(
        parse_toml(no_services)
            .unwrap_err()
            .0
            .iter()
            .any(|error| error.code == "service_required")
    );

    let empty_health_command = r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "test"
[[spec.services]]
name = "web"
[spec.services.source]
type = "image"
image = "alpine"
[spec.services.healthcheck]
type = "command"
command = [""]
"#;
    assert!(
        parse_toml(empty_health_command)
            .unwrap_err()
            .0
            .iter()
            .any(|error| error.code == "healthcheck_command_invalid")
    );
}

#[test]
fn health_paths_resource_limits_and_error_paths_are_safe() {
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
[spec.services.environment]
"token\nvalue" = "bad\u0000value"
[spec.services.healthcheck]
type = "http"
port = 8080
path = "/../ready?token=sensitive"
[spec.services.resources]
"#;
    let errors = parse_toml(input).unwrap_err();
    for code in [
        "environment_name_invalid",
        "environment_value_invalid",
        "healthcheck_path_invalid",
        "resource_limits_empty",
    ] {
        assert!(
            errors.0.iter().any(|error| error.code == code),
            "missing {code}: {errors:?}"
        );
    }
    assert!(
        errors
            .0
            .iter()
            .all(|error| !error.path.contains("token") && !error.message.contains("sensitive"))
    );

    let oversized_memory = serde_json::json!({
        "api_version": "piqueld.dev/v1alpha1",
        "kind": "Application",
        "metadata": { "name": "test" },
        "spec": {
            "services": [{
                "name": "web",
                "source": { "type": "image", "image": "alpine" },
                "resources": { "memory_bytes": u64::MAX }
            }]
        }
    });
    assert!(
        parse_json(&oversized_memory.to_string())
            .unwrap_err()
            .0
            .iter()
            .any(|error| error.code == "memory_limit_invalid")
    );
}

#[test]
fn secret_modes_require_read_only_permission_bits() {
    for mode in ["0000", "0200", "1400", "0480"] {
        let input = format!(
            r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "test"
[[spec.services]]
name = "web"
[spec.services.source]
type = "image"
image = "alpine"
[[spec.services.secrets]]
source = "token"
mode = "{mode}"
"#
        );
        assert!(
            parse_toml(&input)
                .unwrap_err()
                .0
                .iter()
                .any(|error| error.code == "secret_mode_invalid"),
            "mode {mode} should be rejected"
        );
    }
    for mode in ["0400", "0440", "0444", "0500"] {
        let input = format!(
            r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "test"
[[spec.services]]
name = "web"
[spec.services.source]
type = "image"
image = "alpine"
[[spec.services.secrets]]
source = "token"
mode = "{mode}"
"#
        );
        assert!(parse_toml(&input).is_ok(), "mode {mode} should be accepted");
    }
}

#[test]
fn canonical_outputs_repair_order_and_hash_version_is_golden() {
    let mut app = parse_toml(include_str!("fixtures/manifests/git-multi.toml"))
        .unwrap()
        .normalize(id());
    let expected = app.clone();
    app.spec.services.reverse();
    app.spec
        .services
        .iter_mut()
        .find(|service| service.name == "web")
        .unwrap()
        .ports = vec![8080, 8080];
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&app.canonical_json().unwrap()).unwrap(),
        serde_json::from_str::<serde_json::Value>(&expected.canonical_json().unwrap()).unwrap()
    );
    assert_eq!(app.spec_hash(), expected.spec_hash());
    assert_eq!(
        parse_toml(&app.export_toml().unwrap())
            .unwrap()
            .normalize(id()),
        expected
    );
    assert_eq!(
        expected.spec_hash(),
        "sha256:9e490d86210d1db55be21c1bdfa79186a4658f99be28d52a4819d78086462931"
    );

    let other_id = ApplicationId::parse("another-application-id").unwrap();
    let same_spec_other_id = parse_toml(include_str!("fixtures/manifests/git-multi.toml"))
        .unwrap()
        .normalize(other_id);
    assert_eq!(expected.spec_hash(), same_spec_other_id.spec_hash());
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
        let reparsed = parse_toml(&app.export_toml().unwrap()).unwrap().normalize(id());
        prop_assert_eq!(reparsed, app);
    }

    #[test]
    fn generated_resource_names_are_stable_safe_and_bounded(
        logical_name in any::<String>(),
        host in any::<String>(),
        service in any::<String>(),
        port in any::<u16>(),
    ) {
        let resource = docker_resource_name(&id(), ResourceKind::Service, Some(&logical_name));
        prop_assert_eq!(
            &resource,
            &docker_resource_name(&id(), ResourceKind::Service, Some(&logical_name)),
        );
        prop_assert!(resource.len() <= 63);
        prop_assert!(resource.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'));

        let router = router_name(&id(), &host, &service, port);
        prop_assert_eq!(&router, &router_name(&id(), &host, &service, port));
        prop_assert!(router.len() <= 63);
        prop_assert!(router.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'));
    }
}
