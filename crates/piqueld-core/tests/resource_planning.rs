//! Resource compilation and pure planner acceptance tests.

use piqueld_core::planner::{ActionKind, ActionReason, PlanRequest, plan};
use piqueld_core::resource::{
    APPLICATION_LABEL, Convergence, DesiredApplication, INSTANCE_LABEL, InstanceId, MANAGED_LABEL,
    ObservedApplication, ObservedNetwork, ObservedSecret, ObservedService, ObservedVolume,
    ResolutionRequirement, ResolutionSet, ResolvedSource, SERVICE_LABEL, SPEC_HASH_LABEL,
    SecretGeneration, compile_application, preview_resolution,
};
use piqueld_core::{ApplicationId, parse_toml};
use proptest::prelude::*;
use std::collections::BTreeMap;

fn app_id() -> ApplicationId {
    ApplicationId::parse("01jz8r7b4w-test").unwrap()
}
fn instance() -> InstanceId {
    InstanceId::parse("home-1").unwrap()
}

fn image_desired() -> DesiredApplication {
    let app = parse_toml(include_str!("fixtures/manifests/prebuilt.toml"))
        .unwrap()
        .normalize(app_id());
    let resolutions = ResolutionSet {
        sources: BTreeMap::from([(
            "web".into(),
            ResolvedSource::Image {
                requested: "ghcr.io/example/notes:1.4.0".into(),
                digest_reference: format!("ghcr.io/example/notes@sha256:{}", "a".repeat(64)),
            },
        )]),
        secrets: BTreeMap::new(),
    };
    compile_application(&app, instance(), "piqueld-ingress", &resolutions).unwrap()
}

fn git_desired(secret_generation: &str) -> DesiredApplication {
    let app = parse_toml(include_str!("fixtures/manifests/git-multi.toml"))
        .unwrap()
        .normalize(app_id());
    let digest = format!("registry.local/piqueld/shop@sha256:{}", "b".repeat(64));
    let source =
        |service: &str, reference: &str, context: &str, dockerfile: &str| ResolvedSource::Git {
            repository: "https://github.com/example/shop.git".into(),
            requested_reference: reference.into(),
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
            context: context.into(),
            dockerfile: dockerfile.into(),
            registry_reference: format!("registry.local/piqueld/{service}:0123456789ab"),
            digest_reference: digest.clone(),
        };
    let resolutions = ResolutionSet {
        sources: BTreeMap::from([
            (
                "web".into(),
                source("web", "release", "backend", "Containerfile"),
            ),
            ("worker".into(), source("worker", "main", ".", "Dockerfile")),
        ]),
        secrets: BTreeMap::from([(
            "database-url".into(),
            SecretGeneration {
                logical_name: "database-url".into(),
                generation: secret_generation.into(),
                swarm_name: format!("piqueld-secret-database-url-{secret_generation}"),
            },
        )]),
    };
    compile_application(&app, instance(), "piqueld-ingress", &resolutions).unwrap()
}

fn matching(desired: &DesiredApplication) -> ObservedApplication {
    ObservedApplication {
        networks: desired
            .networks
            .iter()
            .map(|v| ObservedNetwork {
                name: v.name.clone(),
                ingress: v.ingress,
                labels: v.labels.clone(),
            })
            .collect(),
        volumes: desired
            .volumes
            .iter()
            .map(|v| ObservedVolume {
                name: v.name.clone(),
                labels: v.labels.clone(),
            })
            .collect(),
        secrets: desired
            .secrets
            .iter()
            .map(|v| ObservedSecret {
                name: v.name.clone(),
                labels: v.labels.clone(),
                in_use: true,
            })
            .collect(),
        services: desired
            .services
            .iter()
            .map(|v| ObservedService {
                name: v.name.clone(),
                image: v.image.clone(),
                replicas: v.replicas,
                environment: v.environment.clone(),
                command: v.command.clone(),
                arguments: v.arguments.clone(),
                mounts: v.mounts.clone(),
                secrets: v.secrets.clone(),
                healthcheck: v.healthcheck.clone(),
                resources: v.resources.clone(),
                networks: v.networks.clone(),
                labels: v.labels.clone(),
                tasks: vec![],
                convergence: Convergence::Converged,
            })
            .collect(),
    }
}

fn action_names(plan: &piqueld_core::Plan) -> Vec<&'static str> {
    plan.actions
        .iter()
        .map(|a| match a.kind {
            ActionKind::ResolveImage { .. } => "resolve_image",
            ActionKind::ResolveGit { .. } => "resolve_git",
            ActionKind::BuildImage { .. } => "build",
            ActionKind::PushImage { .. } => "push",
            ActionKind::EnsureNetwork { .. } => "network",
            ActionKind::EnsureVolume { .. } => "volume",
            ActionKind::EnsureSecret { .. } => "secret",
            ActionKind::EnsureService { .. } => "service",
            ActionKind::WaitForService { .. } => "wait",
            ActionKind::RemoveService { .. } => "remove_service",
            ActionKind::RemoveNetwork { .. } => "remove_network",
            ActionKind::RemoveSecret { .. } => "remove_secret",
            ActionKind::RetainVolume { .. } => "retain_volume",
            ActionKind::AwaitSecretGeneration { .. } => "await_secret",
        })
        .collect()
}

#[test]
fn compilation_has_exact_ownership_and_traefik_labels() {
    let desired = image_desired();
    let service = &desired.services[0];
    let router = piqueld_core::router_name(&desired.id, "notes.example.com", "web", 3000);
    let backend = format!("{router}-backend");
    let expected = BTreeMap::from([
        (MANAGED_LABEL.into(), "true".into()),
        (INSTANCE_LABEL.into(), "home-1".into()),
        (APPLICATION_LABEL.into(), desired.id.to_string()),
        (SERVICE_LABEL.into(), "web".into()),
        (SPEC_HASH_LABEL.into(), desired.spec_hash.clone()),
        ("traefik.enable".into(), "true".into()),
        (
            format!("traefik.http.routers.{router}.entrypoints"),
            "web".into(),
        ),
        (
            format!("traefik.http.routers.{router}.rule"),
            "Host(`notes.example.com`)".into(),
        ),
        (
            format!("traefik.http.routers.{router}.service"),
            backend.clone(),
        ),
        (
            format!("traefik.http.services.{backend}.loadbalancer.server.port"),
            "3000".into(),
        ),
    ]);
    assert_eq!(service.labels, expected);
    assert_eq!(
        service.networks,
        [desired.networks[1].name.clone(), "piqueld-ingress".into()]
    );
    assert!(service.image.contains("@sha256:"));
}

#[test]
fn unresolved_preview_is_explicit_and_apply_compilation_is_blocked() {
    let app = parse_toml(include_str!("fixtures/manifests/git-multi.toml"))
        .unwrap()
        .normalize(app_id());
    let requirements = preview_resolution(&app, &ResolutionSet::default());
    assert!(requirements.iter().any(
        |r| matches!(r, ResolutionRequirement::ResolveGit { service, .. } if service == "web")
    ));
    assert!(requirements.iter().any(
        |r| matches!(r, ResolutionRequirement::BuildAndPush { service } if service == "worker")
    ));
    assert!(requirements.iter().any(|r| matches!(r, ResolutionRequirement::ProvideSecretGeneration { logical_name } if logical_name == "database-url")));
    assert!(
        compile_application(
            &app,
            instance(),
            "piqueld-ingress",
            &ResolutionSet::default()
        )
        .is_err()
    );
    let preview = plan(
        &PlanRequest::Preview {
            unresolved: requirements,
            desired: None,
        },
        &ObservedApplication::default(),
    );
    assert_eq!(preview.summary.mutation_count, 0);
    assert!(action_names(&preview).starts_with(&["resolve_git", "build", "push"]));
}

#[test]
fn create_actions_are_dependency_ordered_and_stably_serialized() {
    let desired = git_desired("g2");
    let request = PlanRequest::Reconcile { desired };
    let first = plan(&request, &ObservedApplication::default());
    let second = plan(&request, &ObservedApplication::default());
    assert_eq!(first, second);
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap()
    );
    assert_eq!(
        action_names(&first),
        [
            "network", "network", "volume", "secret", "service", "wait", "service", "wait"
        ]
    );
    assert_eq!(
        first.actions.iter().map(|a| a.sequence).collect::<Vec<_>>(),
        (1..=8).collect::<Vec<_>>()
    );
}

#[test]
fn matching_state_has_no_actions_even_with_backend_noise() {
    let desired = git_desired("g1");
    let mut observed = matching(&desired);
    observed.services[0]
        .labels
        .insert("com.docker.swarm.internal".into(), "ignored".into());
    observed.services[0].networks.reverse();
    let result = plan(&PlanRequest::Reconcile { desired }, &observed);
    assert!(result.actions.is_empty(), "{result:?}");
    assert!(!result.has_mutations());
}

#[test]
fn owned_service_drift_is_repaired_then_waited_for() {
    let desired = git_desired("g1");
    for field in ["image", "replicas", "environment"] {
        let mut observed = matching(&desired);
        match field {
            "image" => observed.services[0].image = "wrong@sha256:dead".into(),
            "replicas" => observed.services[0].replicas += 1,
            _ => {
                observed.services[0]
                    .environment
                    .insert("DRIFT".into(), "yes".into());
            }
        }
        let result = plan(
            &PlanRequest::Reconcile {
                desired: desired.clone(),
            },
            &observed,
        );
        assert!(matches!(
            result.actions[0].kind,
            ActionKind::EnsureService { .. }
        ));
        assert!(matches!(
            result.actions[0].reason,
            ActionReason::Drift { .. }
        ));
        assert!(matches!(
            result.actions[1].kind,
            ActionKind::WaitForService { .. }
        ));
    }
}

#[test]
fn name_collision_blocks_without_mutating_foreign_resource() {
    let desired = image_desired();
    let mut observed = matching(&desired);
    observed.services[0].labels.clear();
    let result = plan(&PlanRequest::Reconcile { desired }, &observed);
    assert!(result.is_blocked());
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code == "unowned_name_collision")
    );
    assert!(!result.actions.iter().any(|a| matches!(
        a.kind,
        ActionKind::EnsureService { .. } | ActionKind::RemoveService { .. }
    )));
}

#[test]
fn secret_rotation_waits_for_adoption_before_old_generation_cleanup() {
    let old = git_desired("g1");
    let desired = git_desired("g2");
    let mut observed = matching(&old);
    observed.secrets[0].in_use = true;
    let result = plan(
        &PlanRequest::Reconcile {
            desired: desired.clone(),
        },
        &observed,
    );
    assert!(
        result
            .actions
            .iter()
            .any(|a| matches!(a.kind, ActionKind::EnsureSecret { .. }))
    );
    assert!(
        result
            .actions
            .iter()
            .any(|a| matches!(a.reason, ActionReason::SecretGenerationChanged))
    );
    assert!(
        !result
            .actions
            .iter()
            .any(|a| matches!(a.kind, ActionKind::RemoveSecret { .. }))
    );

    let mut adopted = matching(&desired);
    adopted.secrets.push(ObservedSecret {
        name: old.secrets[0].name.clone(),
        labels: old.secrets[0].labels.clone(),
        in_use: false,
    });
    let cleanup = plan(&PlanRequest::Reconcile { desired }, &adopted);
    assert!(
        cleanup
            .actions
            .iter()
            .any(|a| matches!(a.kind, ActionKind::RemoveSecret { .. }))
    );
}

#[test]
fn failed_update_blocks_cleanup_and_preserves_previous_generation() {
    let desired = git_desired("g2");
    let mut observed = matching(&desired);
    observed.services[0].convergence = Convergence::Failed;
    observed.secrets.push(ObservedSecret {
        name: "piqueld-secret-database-url-g1".into(),
        labels: desired.secrets[0].labels.clone(),
        in_use: false,
    });
    let result = plan(&PlanRequest::Reconcile { desired }, &observed);
    assert!(result.is_blocked());
    assert!(
        !result
            .actions
            .iter()
            .any(|a| matches!(a.kind, ActionKind::RemoveSecret { .. }))
    );
}

#[test]
fn deletion_removes_owned_runtime_but_retains_volumes() {
    let desired = git_desired("g1");
    let mut observed = matching(&desired);
    observed.secrets[0].in_use = false;
    let result = plan(
        &PlanRequest::Delete {
            application_id: desired.id,
            instance_id: desired.instance_id,
        },
        &observed,
    );
    assert_eq!(
        action_names(&result),
        [
            "remove_service",
            "remove_service",
            "remove_network",
            "remove_secret",
            "retain_volume"
        ]
    );
    assert!(!result.actions.iter().any(
        |a| matches!(a.kind, ActionKind::RemoveNetwork { ref name } if name == "piqueld-ingress")
    ));
    assert!(
        result
            .actions
            .iter()
            .find(|a| matches!(a.kind, ActionKind::RetainVolume { .. }))
            .is_some_and(|a| !a.mutates_runtime)
    );
}

#[test]
fn removed_volume_is_retained_during_ordinary_reconciliation() {
    let desired = image_desired();
    let mut observed = matching(&desired);
    let owned_labels = desired.networks[1].labels.clone();
    observed.volumes.push(ObservedVolume {
        name: "previous-data".into(),
        labels: owned_labels,
    });
    let result = plan(&PlanRequest::Reconcile { desired }, &observed);
    let retained = result
        .actions
        .iter()
        .find(|action| matches!(action.kind, ActionKind::RetainVolume { .. }))
        .unwrap();
    assert!(!retained.mutates_runtime);
    assert_eq!(retained.human_description(), "RETAIN VOLUME previous-data");
}

proptest! {
    #[test]
    fn planning_is_deterministic_and_does_not_mutate_inputs(extra in "[a-z]{0,12}") {
        let desired = image_desired();
        let mut observed = matching(&desired);
        observed.services[0].labels.insert("runtime.noise".into(), extra);
        let before_desired = desired.clone(); let before_observed = observed.clone();
        let request = PlanRequest::Reconcile { desired: desired.clone() };
        prop_assert_eq!(plan(&request, &observed), plan(&request, &observed));
        prop_assert_eq!(desired, before_desired); prop_assert_eq!(observed, before_observed);
    }
}
