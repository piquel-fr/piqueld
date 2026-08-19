//! Focused resolution and supported-runtime planning coverage.

use piqueld_core::planner::{ActionKind, PlanRequest};
use piqueld_core::resource::{
    Convergence, ObservedApplication, ObservedNetwork, ObservedService, ObservedTask,
    ObservedVolume, ResolutionRequirement, ResolutionSet, ResolvedSource, TaskState,
    compile_application, preview_resolution,
};
use piqueld_core::{ApplicationId, InstanceId, parse_toml};

fn application() -> piqueld_core::NormalizedApplication {
    parse_toml(include_str!("fixtures/manifests/prebuilt.toml"))
        .unwrap()
        .normalize(ApplicationId::parse("app-notes-01").unwrap())
}

fn instance() -> InstanceId {
    InstanceId::parse("instance-test").unwrap()
}

fn resolutions() -> ResolutionSet {
    ResolutionSet {
        sources: [(
            "web".into(),
            ResolvedSource::Image {
                requested: "ghcr.io/example/notes:1.4.0".into(),
                digest_reference: format!("ghcr.io/example/notes@sha256:{}", "a".repeat(64)),
            },
        )]
        .into_iter()
        .collect(),
    }
}

fn observed(desired: &piqueld_core::resource::DesiredApplication) -> ObservedApplication {
    ObservedApplication {
        networks: desired
            .networks
            .iter()
            .map(|network| ObservedNetwork {
                name: network.name.clone(),
                runtime_configuration_matches: true,
                labels: network.labels.clone(),
            })
            .collect(),
        volumes: desired
            .volumes
            .iter()
            .map(|volume| ObservedVolume {
                name: volume.name.clone(),
                runtime_configuration_matches: true,
                labels: volume.labels.clone(),
            })
            .collect(),
        services: desired
            .services
            .iter()
            .map(|service| ObservedService {
                name: service.name.clone(),
                image: service.image.clone(),
                replicas: service.replicas,
                environment: service.environment.clone(),
                command: service.command.clone(),
                arguments: service.arguments.clone(),
                mounts: service.mounts.clone(),
                healthcheck: service.healthcheck.clone(),
                resources: service.resources.clone(),
                networks: service.networks.clone(),
                labels: service.labels.clone(),
                runtime_configuration_matches: true,
                tasks: vec![ObservedTask {
                    state: TaskState::Running,
                    healthy: Some(true),
                    desired_running: true,
                    diagnostic: None,
                }],
                convergence: Convergence::Converged,
            })
            .collect(),
    }
}

#[test]
fn image_resolution_is_the_only_pending_compilation_input() {
    let app = application();
    let missing = ResolutionSet::default();
    assert!(
        matches!(preview_resolution(&app, &missing).as_slice(), [ResolutionRequirement::ResolveImage { service, .. }] if service == "web")
    );
    let desired = compile_application(&app, instance(), &resolutions()).unwrap();
    assert_eq!(
        desired.services[0].image,
        format!("ghcr.io/example/notes@sha256:{}", "a".repeat(64))
    );
    assert!(preview_resolution(&app, &resolutions()).is_empty());
}

#[test]
fn planner_creates_only_supported_runtime_actions() {
    let app = application();
    let desired = compile_application(&app, instance(), &resolutions()).unwrap();
    let plan = piqueld_core::plan(
        &PlanRequest::Reconcile {
            desired: desired.clone(),
        },
        &ObservedApplication::default(),
    );
    assert!(
        plan.actions
            .iter()
            .any(|action| matches!(action.kind, ActionKind::EnsureNetwork { .. }))
    );
    assert!(
        plan.actions
            .iter()
            .any(|action| matches!(action.kind, ActionKind::EnsureService { .. }))
    );
    assert!(
        plan.actions
            .iter()
            .all(|action| !matches!(action.kind, ActionKind::ResolveImage { .. }))
    );
}

#[test]
fn converged_services_need_no_work_and_delete_retains_volumes() {
    let app = application();
    let desired = compile_application(&app, instance(), &resolutions()).unwrap();
    let observed = observed(&desired);
    let plan = piqueld_core::plan(
        &PlanRequest::Reconcile {
            desired: desired.clone(),
        },
        &observed,
    );
    assert!(!plan.has_mutations());
    let deletion = piqueld_core::plan(
        &PlanRequest::Delete {
            application_id: desired.id.clone(),
            instance_id: desired.instance_id.clone(),
        },
        &observed,
    );
    assert!(
        deletion
            .actions
            .iter()
            .any(|action| matches!(action.kind, ActionKind::RetainVolume { .. }))
    );
}
