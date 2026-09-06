//! Focused resolution and supported-runtime planning coverage.

use std::collections::BTreeMap;

use piqueld_core::planner::{ActionKind, PlanRequest};
use piqueld_core::resource::{
    Convergence, ObservedApplication, ObservedNetwork, ObservedService, ObservedTask,
    ObservedVolume, ResolutionRequirement, ResolutionSet, ResolvedSource, TaskState,
    compile_application, image_repository, preview_resolution,
};
use piqueld_core::{ApplicationId, InstanceId, Plan, parse_toml};

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
                healthcheck_configured: service.healthcheck.is_some(),
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
fn docker_registry_aliases_are_canonicalized() {
    assert_eq!(
        image_repository("index.docker.io/library/alpine:3"),
        image_repository("docker.io/library/alpine:3")
    );
    assert_eq!(
        image_repository("LOCALHOST/notes:1"),
        Some("localhost/notes".into())
    );
    assert_eq!(
        image_repository("DoCkEr.Io/alpine:3"),
        Some("docker.io/library/alpine".into())
    );
    assert_eq!(
        image_repository("INDEX.DOCKER.IO/alpine:3"),
        Some("docker.io/library/alpine".into())
    );
}

#[test]
fn planner_creates_only_supported_runtime_actions() {
    let app = application();
    let desired = compile_application(&app, instance(), &resolutions()).unwrap();
    let plan = Plan::from_request(
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
    let plan = Plan::from_request(
        &PlanRequest::Reconcile {
            desired: desired.clone(),
        },
        &observed,
    );
    assert!(!plan.has_mutations());
    let deletion = Plan::from_request(
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
    assert!(deletion.actions.iter().any(|action| {
        matches!(action.kind, ActionKind::RemoveService { .. }) && action.destructive
    }));
    assert!(deletion.summary.destructive_count > 0);

    let mut drifted = observed.clone();
    drifted.services[0].runtime_configuration_matches = false;
    let drift_plan = Plan::from_request(
        &PlanRequest::Reconcile {
            desired: desired.clone(),
        },
        &drifted,
    );
    assert!(
        drift_plan
            .actions
            .iter()
            .any(|action| { matches!(action.kind, ActionKind::EnsureService { .. }) })
    );

    let mut foreign = observed;
    foreign.services[0]
        .labels
        .insert("io.piqueld.instance".into(), "other-instance".into());
    let foreign_deletion = Plan::from_request(
        &PlanRequest::Delete {
            application_id: desired.id,
            instance_id: desired.instance_id,
        },
        &foreign,
    );
    assert!(
        !foreign_deletion
            .actions
            .iter()
            .any(|action| { matches!(action.kind, ActionKind::RemoveService { .. }) })
    );
    assert!(foreign_deletion.is_blocked());
    assert!(foreign_deletion.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == piqueld_core::codes::UNOWNED_NAME_COLLISION && diagnostic.blocking
    }));
}

// ---------------------------------------------------------------------------
// Extended planning coverage restored from the pre-06A suite.
// ---------------------------------------------------------------------------

use piqueld_core::planner::{ActionReason, ActionRisk, DiagnosticSeverity};
use piqueld_core::resource::{
    DesiredNetwork, DesiredService, DesiredVolume, OwnershipState, Sha256Digest,
};

fn digest() -> String {
    format!("sha256:{}", "a".repeat(64))
}

fn labels_for(
    instance: &InstanceId,
    application: &ApplicationId,
) -> std::collections::BTreeMap<String, String> {
    std::collections::BTreeMap::from([
        ("io.piqueld.managed".into(), "true".into()),
        ("io.piqueld.instance".into(), instance.to_string()),
        ("io.piqueld.application".into(), application.to_string()),
        ("io.piqueld.spec-hash".into(), digest()),
    ])
}

#[test]
fn desired_identity_matrices_reject_non_canonical_resources() {
    let id = ApplicationId::parse("app-notes-01").unwrap();
    let instance = instance();
    let network = DesiredNetwork {
        name: piqueld_core::docker_resource_name(&id, piqueld_core::ResourceKind::Network, None),
        labels: labels_for(&instance, &id),
    };
    assert!(network.has_valid_identity());

    // A service label on a network breaks the role separation.
    let mut mislabeled = network.clone();
    mislabeled
        .labels
        .insert("io.piqueld.service".into(), "web".into());
    assert!(!mislabeled.has_valid_identity());

    // An off-canonical name is rejected.
    let mut renamed = network.clone();
    renamed.name = format!("{}x", renamed.name);
    assert!(!renamed.has_valid_identity());

    let volume = DesiredVolume {
        logical_name: "data".into(),
        name: piqueld_core::docker_resource_name(
            &id,
            piqueld_core::ResourceKind::Volume,
            Some("data"),
        ),
        labels: labels_for(&instance, &id),
    };
    assert!(volume.has_valid_identity());

    let resolved = resolutions().sources.get("web").unwrap().clone();
    let service = DesiredService {
        logical_name: "web".into(),
        name: piqueld_core::docker_resource_name(
            &id,
            piqueld_core::ResourceKind::Service,
            Some("web"),
        ),
        source: resolved,
        image: format!("ghcr.io/example/notes@{}", digest()),
        replicas: 1,
        environment: BTreeMap::default(),
        command: Vec::new(),
        arguments: Vec::new(),
        mounts: Vec::new(),
        healthcheck: None,
        resources: None,
        networks: vec![piqueld_core::docker_resource_name(
            &id,
            piqueld_core::ResourceKind::Network,
            None,
        )],
        labels: {
            let mut labels = labels_for(&instance, &id);
            labels.insert("io.piqueld.service".into(), "web".into());
            labels
        },
    };
    assert!(service.has_valid_identity());

    let mut wrong_service_label = service.clone();
    wrong_service_label
        .labels
        .insert("io.piqueld.service".into(), "other".into());
    assert!(!wrong_service_label.has_valid_identity());
}

#[test]
fn ownership_states_classify_labels() {
    let id = ApplicationId::parse("app-notes-01").unwrap();
    let instance = instance();

    assert_eq!(
        OwnershipState::from_labels(&labels_for(&instance, &id), &instance, &id),
        OwnershipState::Owned
    );

    let foreign = OwnershipState::from_labels(
        &labels_for(&InstanceId::parse("other-instance").unwrap(), &id),
        &instance,
        &id,
    );
    assert_eq!(foreign, OwnershipState::Foreign);

    for mutation in [
        ("io.piqueld.managed", "false"),
        ("io.piqueld.spec-hash", "sha256:nothex"),
    ] {
        let mut invalid = labels_for(&instance, &id);
        invalid.insert(mutation.0.into(), mutation.1.into());
        assert_eq!(
            OwnershipState::from_labels(&invalid, &instance, &id),
            OwnershipState::Invalid
        );
    }

    let missing_hash = labels_for(&instance, &id);
    let missing_hash = missing_hash
        .into_iter()
        .filter(|(key, _)| key != "io.piqueld.spec-hash")
        .collect();
    assert_eq!(
        OwnershipState::from_labels(&missing_hash, &instance, &id),
        OwnershipState::Invalid
    );
}

#[test]
fn drift_fields_are_granular_for_command_and_arguments() {
    let app = application();
    let desired = compile_application(&app, instance(), &resolutions()).unwrap();
    let mut drifted = observed(&desired);
    drifted.services[0].command = vec!["sh".into()];
    drifted.services[0].arguments = vec!["-c".into(), "true".into()];
    let plan = Plan::from_request(
        &PlanRequest::Reconcile {
            desired: desired.clone(),
        },
        &drifted,
    );
    let reasons = plan
        .actions
        .iter()
        .filter_map(|action| match &action.reason {
            ActionReason::Drift { fields } => Some(fields.clone()),
            _ => None,
        })
        .flatten()
        .collect::<Vec<_>>();
    assert!(reasons.contains(&"command".to_string()), "{reasons:?}");
    assert!(reasons.contains(&"arguments".to_string()), "{reasons:?}");
    assert!(!reasons.contains(&"process".to_string()));
}

#[test]
fn obsolete_cleanup_is_gated_and_reports_deferred_diagnostics() {
    let app = application();
    let desired = compile_application(&app, instance(), &resolutions()).unwrap();
    // Build a fully-converged observation, then add an obsolete owned service
    // while breaking convergence of the wanted service so cleanup stays gated.
    let base = observed(&desired);
    let mut gated = base.clone();
    gated.services[0].convergence = Convergence::Updating;
    let mut stale = gated.services[0].clone();
    stale.name = piqueld_core::docker_resource_name(
        &desired.id,
        piqueld_core::ResourceKind::Service,
        Some("web-stale"),
    );
    stale
        .labels
        .insert("io.piqueld.service".into(), "web-stale".into());
    gated.services.push(stale);
    let plan = Plan::from_request(
        &PlanRequest::Reconcile {
            desired: desired.clone(),
        },
        &gated,
    );
    assert!(
        !plan
            .actions
            .iter()
            .any(|action| matches!(action.kind, ActionKind::RemoveService { .. })),
        "cleanup must stay gated while services are not ready"
    );
    assert!(
        plan.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "cleanup_deferred"
                && diagnostic.severity == DiagnosticSeverity::Info),
        "{:?}",
        plan.diagnostics
    );

    // Once everything converges, the stale service is removed.
    let ready = observed(&desired);
    let mut converged_with_stale = ready.clone();
    let mut stale_ready = ready.services[0].clone();
    stale_ready.name = piqueld_core::docker_resource_name(
        &desired.id,
        piqueld_core::ResourceKind::Service,
        Some("web-stale"),
    );
    stale_ready
        .labels
        .insert("io.piqueld.service".into(), "web-stale".into());
    converged_with_stale.services.push(stale_ready);
    let plan = Plan::from_request(
        &PlanRequest::Reconcile {
            desired: desired.clone(),
        },
        &converged_with_stale,
    );
    assert!(
        plan.actions
            .iter()
            .any(|action| matches!(action.kind, ActionKind::RemoveService { .. }))
    );
}

#[test]
fn failed_convergence_blocks_with_a_service_update_diagnostic() {
    let app = application();
    let desired = compile_application(&app, instance(), &resolutions()).unwrap();
    let mut failed = observed(&desired);
    failed.services[0].convergence = Convergence::Failed;
    let plan = Plan::from_request(&PlanRequest::Reconcile { desired }, &failed);
    assert!(plan.is_blocked());
    let diagnostic = plan
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "service_update_failed")
        .expect("blocking update-failure diagnostic");
    assert_eq!(diagnostic.severity, DiagnosticSeverity::Error);
}

#[test]
fn preview_plans_splice_resolution_actions_first() {
    let app = application();
    let unresolved = preview_resolution(&app, &ResolutionSet::default());
    let plan = Plan::from_request(
        &PlanRequest::Preview {
            unresolved,
            desired: None,
        },
        &ObservedApplication::default(),
    );
    assert!(matches!(
        plan.actions.first().map(|a| &a.kind),
        Some(ActionKind::ResolveImage { .. })
    ));
    assert!(!plan.is_blocked());

    let desired = compile_application(&app, instance(), &resolutions()).unwrap();
    let with_desired = Plan::from_request(
        &PlanRequest::Preview {
            unresolved: Vec::new(),
            desired: Some(desired),
        },
        &ObservedApplication::default(),
    );
    assert!(with_desired.summary.mutation_count > 0);
}

#[test]
fn operation_steps_are_bounded_stable_and_deterministic() {
    use piqueld_core::PlanAction;
    let short = PlanAction::wait_for_service("web");
    assert!(short.operation_step().len() <= 64);
    assert_eq!(short.operation_step(), short.operation_step());

    let long = PlanAction::new(
        ActionKind::EnsureVolume {
            volume: DesiredVolume {
                logical_name: "data".repeat(40),
                name:
                    "very-long-volume-name-that-goes-on-and-on-and-never-seems-to-stop-being-long"
                        .into(),
                labels: std::collections::BTreeMap::default(),
            },
        },
        ActionReason::Missing,
        ActionRisk::DataAdjacent,
        true,
    );
    let step = long.operation_step();
    assert!(step.len() <= 64, "{step}");
    assert!(step.contains('~'), "{step}");
    assert_eq!(step, long.operation_step());
}

#[test]
fn wire_decoding_enforces_domain_guards() {
    // TaskState unknown strings decode to Unknown.
    let task: TaskState = serde_json::from_str("\"some-future-state\"").unwrap();
    assert_eq!(task, TaskState::Unknown);

    // ResolvedSource digest references must be immutable repository digests.
    let good = serde_json::json!({
        "type": "image",
        "requested": "nginx:1",
        "digest_reference": format!("docker.io/library/nginx@{}", digest()),
    });
    let decoded: ResolvedSource = serde_json::from_value(good).unwrap();
    assert!(decoded.digest_reference().ends_with(&digest()));

    let bad = serde_json::json!({
        "type": "image",
        "requested": "nginx:1",
        "digest_reference": "docker.io/library/nginx:1",
    });
    assert!(serde_json::from_value::<ResolvedSource>(bad).is_err());

    // PlanAction destructive/risk must agree.
    let consistent = serde_json::json!({
        "sequence": 1,
        "kind": {"action": "retain_volume", "name": "vol"},
        "reason": {"reason": "volume_retention_policy"},
        "risk": "none",
        "mutates_runtime": false,
        "destructive": false,
    });
    assert!(serde_json::from_value::<piqueld_core::PlanAction>(consistent).is_ok());

    let inconsistent = serde_json::json!({
        "sequence": 1,
        "kind": {"action": "retain_volume", "name": "vol"},
        "reason": {"reason": "volume_retention_policy"},
        "risk": "destructive",
        "mutates_runtime": true,
        "destructive": false,
    });
    assert!(serde_json::from_value::<piqueld_core::PlanAction>(inconsistent).is_err());
}

#[test]
fn observation_matching_ignores_order_but_not_multiplicity() {
    let app = application();
    let desired = compile_application(&app, instance(), &resolutions()).unwrap();

    // Order is irrelevant for networks and mounts alike...
    let mut reordered = observed(&desired);
    reordered.services[0].mounts.reverse();
    assert!(reordered.services[0].matches(&desired.services[0]));

    // ...but multiplicity is preserved: duplicated attachments are drift.
    let mut duplicated = observed(&desired);
    let networks = desired.services[0].networks.clone();
    duplicated.services[0].networks = networks
        .iter()
        .flat_map(|network| [network.clone(), network.clone()])
        .collect();
    assert!(!duplicated.services[0].matches(&desired.services[0]));
}

#[test]
fn compiled_ownership_carries_the_spec_hash_label() {
    let app = application();
    let desired = compile_application(&app, instance(), &resolutions()).unwrap();
    let hash = Sha256Digest::parse(desired.spec_hash.clone()).unwrap();
    for network in &desired.networks {
        assert_eq!(
            network
                .labels
                .get("io.piqueld.spec-hash")
                .map(String::as_str),
            Some(hash.as_str())
        );
    }
}
