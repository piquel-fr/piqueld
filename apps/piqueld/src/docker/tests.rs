use super::*;
use piqueld_core::resource::{DesiredMount, ResolvedSource};
use std::error::Error as _;

fn desired() -> DesiredService {
    let digest = format!("docker.io/library/alpine@sha256:{}", "a".repeat(64));
    let application = ApplicationId::parse("app-example").unwrap();
    DesiredService {
        logical_name: "web".into(),
        name: docker_resource_name(&application, ResourceKind::Service, Some("web")),
        source: ResolvedSource::Image {
            requested: "alpine:3".into(),
            digest_reference: digest.clone(),
        },
        image: digest,
        replicas: 2,
        environment: BTreeMap::from([("RUST_LOG".into(), "info".into())]),
        command: vec!["/bin/server".into()],
        arguments: vec!["--listen".into()],
        ports: vec![],
        mounts: vec![DesiredMount {
            volume_name: "piqueld-app-example-data".into(),
            target: "/data".into(),
            read_only: false,
        }],
        secrets: vec![],
        healthcheck: Some(HealthCheck::Command {
            command: vec!["/bin/true".into()],
            interval_seconds: 10,
            timeout_seconds: 2,
        }),
        resources: Some(ResourceLimits {
            cpu_millis: Some(500),
            memory_bytes: Some(1024),
        }),
        networks: vec!["piqueld-app-example".into()],
        labels: BTreeMap::from([
            ("io.piqueld.managed".into(), "true".into()),
            ("io.piqueld.instance".into(), "instance-1".into()),
            ("io.piqueld.application".into(), "app-example".into()),
            ("io.piqueld.service".into(), "web".into()),
            (
                "io.piqueld.spec-hash".into(),
                format!("sha256:{}", "b".repeat(64)),
            ),
        ]),
    }
}

#[test]
fn docker_hub_short_references_use_the_library_repository() {
    assert_eq!(
        BollardDocker::image_repository("docker.io/alpine:3"),
        Some("docker.io/library/alpine".into())
    );
    assert_eq!(
        BollardDocker::image_repository("docker.io/example/alpine:3"),
        Some("docker.io/example/alpine".into())
    );
}

#[test]
fn operation_errors_retain_sanitized_docker_context() {
    let docker_error = DockerError::request(
        "list services",
        bollard::errors::Error::DockerResponseServerError {
            status_code: 500,
            message: "engine failure detail".into(),
        },
    );
    assert!(docker_error.source().is_some());
    let error = OperationError::from(docker_error);

    assert_eq!(error, OperationError::DockerRequestFailed("list services"));
    assert_eq!(
        error.tuple(),
        (
            "docker_request_failed",
            "Docker request failed while list services".into()
        )
    );
}

#[test]
fn service_edge_generates_safe_replicated_rolling_spec() {
    let spec = BollardDocker::service_spec(&desired()).unwrap();
    let update = spec.update_config.unwrap();
    assert_eq!(
        update.failure_action,
        Some(ServiceSpecUpdateConfigFailureActionEnum::PAUSE)
    );
    assert_eq!(
        update.order,
        Some(ServiceSpecUpdateConfigOrderEnum::START_FIRST)
    );
    assert_eq!(spec.mode.unwrap().replicated.unwrap().replicas, Some(2));
    let task = spec.task_template.unwrap();
    assert_eq!(
        task.resources.unwrap().limits.unwrap().nano_cpus,
        Some(500_000_000)
    );
    let container = task.container_spec.unwrap();
    assert_eq!(
        container.mounts.unwrap()[0].typ,
        Some(MountTypeEnum::VOLUME)
    );
    assert_eq!(container.health_check.unwrap().test.unwrap()[0], "CMD");
}

#[test]
fn service_ports_round_trip_without_host_publication() {
    let mut desired = desired();
    desired.ports = vec![8080, 3000];

    let spec = BollardDocker::service_spec(&desired).unwrap();
    let endpoint = spec.endpoint_spec.as_ref().unwrap();
    assert_eq!(endpoint.mode, Some(EndpointSpecModeEnum::VIP));
    assert_eq!(
        endpoint
            .ports
            .as_ref()
            .unwrap()
            .iter()
            .map(|port| (port.target_port, port.published_port, port.publish_mode))
            .collect::<Vec<_>>(),
        vec![(Some(8080), None, None), (Some(3000), None, None)]
    );

    let observed = BollardDocker::observe_service(&spec, Vec::new(), None).unwrap();
    assert_eq!(observed.ports, [3000, 8080]);
    assert!(observed.runtime_configuration_matches);
}
#[test]
fn swarm_topology_requires_exactly_one_ready_manager() {
    let manager = bollard::models::Node {
        spec: Some(bollard::models::NodeSpec {
            role: Some(bollard::models::NodeSpecRoleEnum::MANAGER),
            availability: Some(bollard::models::NodeSpecAvailabilityEnum::ACTIVE),
            ..Default::default()
        }),
        status: Some(bollard::models::NodeStatus {
            state: Some(bollard::models::NodeState::READY),
            ..Default::default()
        }),
        manager_status: Some(bollard::models::ManagerStatus {
            reachability: Some(bollard::models::Reachability::REACHABLE),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(BollardDocker::single_node_manager(std::slice::from_ref(
        &manager
    )));
    assert!(!BollardDocker::single_node_manager(&[]));
    assert!(!BollardDocker::single_node_manager(&[
        manager.clone(),
        manager.clone()
    ]));
    let mut worker = manager;
    worker.spec.as_mut().unwrap().role = Some(bollard::models::NodeSpecRoleEnum::WORKER);
    assert!(!BollardDocker::single_node_manager(&[worker]));
}

#[test]
fn swarm_topology_rejects_drained_or_unreachable_managers() {
    let mut manager = bollard::models::Node {
        spec: Some(bollard::models::NodeSpec {
            role: Some(bollard::models::NodeSpecRoleEnum::MANAGER),
            availability: Some(bollard::models::NodeSpecAvailabilityEnum::DRAIN),
            ..Default::default()
        }),
        status: Some(bollard::models::NodeStatus {
            state: Some(bollard::models::NodeState::READY),
            ..Default::default()
        }),
        manager_status: Some(bollard::models::ManagerStatus {
            reachability: Some(bollard::models::Reachability::REACHABLE),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(!BollardDocker::single_node_manager(std::slice::from_ref(
        &manager
    )));
    manager.spec.as_mut().unwrap().availability =
        Some(bollard::models::NodeSpecAvailabilityEnum::ACTIVE);
    manager.manager_status.as_mut().unwrap().reachability =
        Some(bollard::models::Reachability::UNREACHABLE);
    assert!(!BollardDocker::single_node_manager(&[manager]));
}
#[test]
fn service_edge_rejects_mutable_images_and_secret_mounts() {
    let mut service = desired();
    service.image = "alpine:latest".into();
    assert_eq!(
        BollardDocker::service_spec(&service).unwrap_err(),
        DockerError::Request("build service specification")
    );
}

#[test]
fn service_edge_rejects_memory_limits_outside_docker_range() {
    let mut service = desired();
    service.resources.as_mut().unwrap().memory_bytes = Some(u64::MAX);
    assert_eq!(
        BollardDocker::service_spec(&service).unwrap_err(),
        DockerError::Request("build service specification")
    );
}
#[test]
fn ownership_validation_allows_spec_drift_but_not_foreign_instance() {
    let desired = desired();
    let mut old = desired.labels.clone();
    old.insert(
        "io.piqueld.spec-hash".into(),
        format!("sha256:{}", "c".repeat(64)),
    );
    assert!(BollardDocker::owns(&old, &desired.labels));
    old.insert("io.piqueld.instance".into(), "another".into());
    assert!(!BollardDocker::owns(&old, &desired.labels));
}

#[test]
fn destructive_ownership_requires_a_valid_hash_and_resource_identity() {
    let desired = desired();
    let mut base = desired.labels.clone();
    base.remove(SERVICE_LABEL);
    assert!(BollardDocker::owns_named_service(
        &desired.labels,
        &base,
        &desired.name
    ));

    let mut missing_hash = desired.labels.clone();
    missing_hash.remove(SPEC_HASH_LABEL);
    assert!(!BollardDocker::owns_named_service(
        &missing_hash,
        &base,
        &desired.name
    ));

    let mut missing_service = desired.labels.clone();
    missing_service.remove(SERVICE_LABEL);
    assert!(!BollardDocker::owns_named_service(
        &missing_service,
        &base,
        &desired.name
    ));
    assert!(!BollardDocker::owns_named_service(
        &desired.labels,
        &base,
        "piqueld-app-example-worker"
    ));
}

#[test]
fn mutation_inputs_require_deterministic_resource_identity() {
    let service = desired();
    assert!(service.has_valid_identity());
    let application = ApplicationId::parse("app-example").unwrap();
    let network = DesiredNetwork {
        name: docker_resource_name(&application, ResourceKind::Network, None),
        ingress: false,
        labels: service
            .labels
            .iter()
            .filter(|(key, _)| key.as_str() != SERVICE_LABEL)
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    };
    assert!(network.has_valid_identity());
    let volume = DesiredVolume {
        logical_name: "data".into(),
        name: docker_resource_name(&application, ResourceKind::Volume, Some("data")),
        labels: network.labels.clone(),
    };
    assert!(volume.has_valid_identity());

    let mut wrong_service = service;
    wrong_service.name = "piqueld-app-example-service-worker".into();
    assert!(!wrong_service.has_valid_identity());
    let mut wrong_volume = volume;
    wrong_volume.logical_name = "other".into();
    assert!(!wrong_volume.has_valid_identity());
}

#[test]
fn observation_name_filter_covers_truncated_application_identities() {
    let application = ApplicationId::parse("a".repeat(64)).unwrap();
    let name = docker_resource_name(&application, ResourceKind::Service, Some("web"));
    assert!(BollardDocker::relevant(
        &name,
        &BTreeMap::new(),
        &application
    ));
}

#[test]
fn generated_service_round_trips_to_a_matching_canonical_observation() {
    let desired = desired();
    let spec = BollardDocker::service_spec(&desired).unwrap();
    let observed = BollardDocker::observe_service(&spec, Vec::new(), None).unwrap();
    assert!(observed.semantically_matches(&desired));

    let mut normalized = BollardDocker::service_spec(&desired).unwrap();
    normalized
        .task_template
        .as_mut()
        .unwrap()
        .container_spec
        .as_mut()
        .unwrap()
        .stop_grace_period = Some(STOP_GRACE_PERIOD);
    normalized.rollback_config = Some(ServiceSpecRollbackConfig {
        parallelism: Some(1),
        delay: None,
        failure_action: Some(ServiceSpecRollbackConfigFailureActionEnum::PAUSE),
        monitor: Some(ROLLBACK_MONITOR),
        max_failure_ratio: Some(0.0),
        order: Some(ServiceSpecRollbackConfigOrderEnum::STOP_FIRST),
    });
    normalized.update_config.as_mut().unwrap().delay = None;
    let observed = BollardDocker::observe_service(&normalized, Vec::new(), None).unwrap();
    assert!(observed.semantically_matches(&desired));
}

#[test]
fn weakened_runtime_policy_and_unsupported_mounts_are_observed_as_drift() {
    let desired = desired();
    let mut spec = BollardDocker::service_spec(&desired).unwrap();
    spec.update_config.as_mut().unwrap().failure_action =
        Some(ServiceSpecUpdateConfigFailureActionEnum::CONTINUE);
    let observed = BollardDocker::observe_service(&spec, Vec::new(), None).unwrap();
    assert!(!observed.runtime_configuration_matches);
    assert!(!observed.semantically_matches(&desired));

    let mut spec = BollardDocker::service_spec(&desired).unwrap();
    spec.task_template
        .as_mut()
        .unwrap()
        .container_spec
        .as_mut()
        .unwrap()
        .mounts
        .as_mut()
        .unwrap()
        .push(Mount {
            typ: Some(MountTypeEnum::BIND),
            source: Some("/host".into()),
            target: Some("/container".into()),
            ..Default::default()
        });
    let observed = BollardDocker::observe_service(&spec, Vec::new(), None).unwrap();
    assert!(!observed.runtime_configuration_matches);

    let mut spec = BollardDocker::service_spec(&desired).unwrap();
    spec.task_template.as_mut().unwrap().force_update = Some(1);
    spec.endpoint_spec = Some(bollard::models::EndpointSpec {
        mode: Some(bollard::models::EndpointSpecModeEnum::DNSRR),
        ..Default::default()
    });
    let observed = BollardDocker::observe_service(&spec, Vec::new(), None).unwrap();
    assert!(!observed.runtime_configuration_matches);

    let mut spec = BollardDocker::service_spec(&desired).unwrap();
    spec.task_template
        .as_mut()
        .unwrap()
        .container_spec
        .as_mut()
        .unwrap()
        .env = Some(vec!["RUST_LOG=debug".into(), "RUST_LOG=info".into()]);
    let observed = BollardDocker::observe_service(&spec, Vec::new(), None).unwrap();
    assert!(!observed.runtime_configuration_matches);

    let mut spec = BollardDocker::service_spec(&desired).unwrap();
    let health = spec
        .task_template
        .as_mut()
        .unwrap()
        .container_spec
        .as_mut()
        .unwrap()
        .health_check
        .as_mut()
        .unwrap();
    health.start_period = Some(NANOSECONDS_PER_SECOND);
    let observed = BollardDocker::observe_service(&spec, Vec::new(), None).unwrap();
    assert!(!observed.runtime_configuration_matches);
}

#[test]
fn convergence_waits_for_docker_update_completion() {
    let task = ObservedTask {
        state: TaskState::Running,
        healthy: None,
        desired_running: true,
        diagnostic: None,
    };
    let spec = BollardDocker::service_spec(&desired()).unwrap();
    let observed = BollardDocker::observe_service(
        &spec,
        vec![task.clone(), task],
        Some(bollard::models::ServiceUpdateStatusStateEnum::UPDATING),
    )
    .unwrap();
    assert_eq!(observed.convergence, Convergence::Updating);
}

#[test]
fn task_failures_expose_only_structured_sanitized_diagnostics() {
    let task = bollard::models::Task {
        desired_state: Some(bollard::models::TaskState::RUNNING),
        status: Some(bollard::models::TaskStatus {
            state: Some(bollard::models::TaskState::FAILED),
            err: Some("registry-token-canary".into()),
            message: Some("secret-message-canary".into()),
            container_status: Some(bollard::models::ContainerStatus {
                exit_code: Some(17),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let observed = BollardDocker::observe_task(&task);
    assert_eq!(
        observed.diagnostic,
        Some(TaskDiagnostic::Failed {
            exit_code: Some(17)
        })
    );
    let json = serde_json::to_string(&observed).unwrap();
    assert!(!json.contains("canary"));
}
