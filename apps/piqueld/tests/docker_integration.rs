//! Privileged end-to-end qualification for the Docker adapter.

use bollard::query_parameters::{InspectServiceOptions, UpdateServiceOptionsBuilder};
use piqueld::docker::{BollardDocker, DockerApi, DockerError};
use piqueld_core::manifest::HealthCheck;
use piqueld_core::resource::{DesiredNetwork, DesiredService, DesiredVolume, ResolvedSource};
use piqueld_core::{ApplicationId, InstanceId, ResourceKind, docker_resource_name};
use std::{collections::BTreeMap, path::Path, time::Duration};

/// Ensures that a Docker service reaches its desired state, retrying transient request failures for up to ten seconds.
///
/// # Panics
///
/// Panics if ensuring the service fails with a non-request error or remains unsuccessful after the retry period.
///
/// # Examples
///
/// ```no_run
/// # async fn example(docker: &BollardDocker, desired: &DesiredService) {
/// ensure_service_eventually(docker, desired).await;
/// # }
/// ```
async fn ensure_service_eventually(docker: &BollardDocker, desired: &DesiredService) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match docker.ensure_service(desired).await {
            Ok(()) => return,
            Err(DockerError::Request(_)) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => panic!("failed to ensure Docker service: {error}"),
        }
    }
}

/// Privileged lifecycle qualification. It is ignored so ordinary
/// developer/CI runs never mutate the host Docker Engine.
#[tokio::test]
#[ignore = "requires an isolated privileged Docker Engine"]
// Keep the privileged lifecycle scenario linear so each mutation and assertion
// remains visible in one end-to-end test.
#[allow(clippy::too_many_lines)]
async fn swarm_init_create_replica_drift_restart_delete_and_volume_retention() {
    assert_eq!(
        std::env::var("PIQUELD_DOCKER_ISOLATED").as_deref(),
        Ok("1"),
        "refusing to mutate Docker without an explicit isolated-daemon attestation"
    );
    let socket = std::env::var("PIQUELD_DOCKER_SOCKET")
        .expect("PIQUELD_DOCKER_SOCKET must point at an isolated privileged daemon");
    let docker = BollardDocker::connect(Path::new(&socket)).unwrap();
    docker.ensure_swarm(true).await.unwrap();
    let suffix = uuid::Uuid::now_v7().simple().to_string();
    let app = ApplicationId::parse(format!("app-{}", &suffix[..16])).unwrap();
    let instance = InstanceId::parse("integration-instance").unwrap();
    let spec_hash = format!("sha256:{}", "a".repeat(64));
    let labels = BTreeMap::from([
        ("io.piqueld.managed".into(), "true".into()),
        ("io.piqueld.instance".into(), instance.to_string()),
        ("io.piqueld.application".into(), app.to_string()),
        ("io.piqueld.spec-hash".into(), spec_hash),
    ]);
    let network = DesiredNetwork {
        name: docker_resource_name(&app, ResourceKind::Network, None),
        labels: labels.clone(),
    };
    let volume = DesiredVolume {
        logical_name: "data".into(),
        name: docker_resource_name(&app, ResourceKind::Volume, Some("data")),
        labels: labels.clone(),
    };
    docker.ensure_network(&network).await.unwrap();
    docker.ensure_volume(&volume).await.unwrap();
    let image = docker.resolve_image("alpine:3.20").await.unwrap();
    let mut service_labels = labels.clone();
    service_labels.insert("io.piqueld.service".into(), "web".into());
    let mut service = DesiredService {
        logical_name: "web".into(),
        name: docker_resource_name(&app, ResourceKind::Service, Some("web")),
        source: ResolvedSource::Image {
            requested: "alpine:3.20".into(),
            digest_reference: image.clone(),
        },
        image,
        replicas: 1,
        environment: BTreeMap::new(),
        command: vec!["/bin/sh".into()],
        arguments: vec!["-c".into(), "while true; do sleep 5; done".into()],
        mounts: vec![],
        healthcheck: Some(HealthCheck::Command {
            command: vec!["true".into()],
            interval_seconds: 1,
            timeout_seconds: 1,
        }),
        resources: None,
        networks: vec![network.name.clone()],
        labels: service_labels,
    };
    ensure_service_eventually(&docker, &service).await;

    let mut http_service = service.clone();
    http_service.logical_name = "http".into();
    http_service.name = docker_resource_name(&app, ResourceKind::Service, Some("http"));
    http_service.command = vec!["/bin/sh".into()];
    http_service.arguments = vec![
        "-c".into(),
        "mkdir -p /www && printf healthy >/www/health && httpd -f -p 8080 -h /www".into(),
    ];
    http_service.healthcheck = Some(HealthCheck::Http {
        port: 8080,
        path: "/health".into(),
        interval_seconds: 1,
        timeout_seconds: 1,
    });
    http_service
        .labels
        .insert("io.piqueld.service".into(), "http".into());
    ensure_service_eventually(&docker, &http_service).await;

    let observed = docker.observe(&app).await.unwrap();
    assert_eq!(
        observed
            .services
            .iter()
            .find(|candidate| candidate.name == service.name)
            .and_then(|candidate| candidate.healthcheck.as_ref()),
        service.healthcheck.as_ref(),
        "command health check survives complete service inspection"
    );
    assert_eq!(
        observed
            .services
            .iter()
            .find(|candidate| candidate.name == http_service.name)
            .and_then(|candidate| candidate.healthcheck.as_ref()),
        http_service.healthcheck.as_ref(),
        "HTTP health check survives complete service inspection"
    );

    service.replicas = 2;
    ensure_service_eventually(&docker, &service).await;
    // Reconnecting exercises the same observation/recovery seam used after daemon restart.
    let restarted = BollardDocker::connect(Path::new(&socket)).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_mins(1);
    loop {
        let observed = restarted.observe(&app).await.unwrap();
        if observed
            .services
            .iter()
            .any(|s| s.name == service.name && s.replicas == 2)
        {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let observed = restarted.observe(&app).await.unwrap();
    assert_eq!(
        observed
            .services
            .iter()
            .find(|candidate| candidate.name == service.name)
            .and_then(|candidate| candidate.healthcheck.as_ref()),
        service.healthcheck.as_ref()
    );
    let raw =
        bollard::Docker::connect_with_unix(&socket, 120, bollard::API_DEFAULT_VERSION).unwrap();
    let matching = raw
        .inspect_service(&service.name, None::<InspectServiceOptions>)
        .await
        .unwrap();
    let matching_version = matching.version.as_ref().and_then(|version| version.index);
    ensure_service_eventually(&restarted, &service).await;
    let unchanged = raw
        .inspect_service(&service.name, None::<InspectServiceOptions>)
        .await
        .unwrap();
    assert_eq!(
        unchanged.version.and_then(|version| version.index),
        matching_version,
        "a matching reconcile must not update the Docker service"
    );

    // Make an owned service drift through the raw API, then verify the adapter repairs it.
    let mut drifted_spec = matching.spec.unwrap();
    drifted_spec
        .mode
        .as_mut()
        .unwrap()
        .replicated
        .as_mut()
        .unwrap()
        .replicas = Some(1);
    raw.update_service(
        &service.name,
        drifted_spec,
        UpdateServiceOptionsBuilder::default()
            .version(i32::try_from(matching_version.unwrap()).unwrap())
            .build(),
        None,
    )
    .await
    .unwrap();
    restarted.ensure_service(&service).await.unwrap();
    let repaired = raw
        .inspect_service(&service.name, None::<InspectServiceOptions>)
        .await
        .unwrap();
    assert!(
        repaired
            .version
            .and_then(|version| version.index)
            .is_some_and(|version| matching_version.is_some_and(|previous| version > previous))
    );
    assert_eq!(
        repaired
            .spec
            .and_then(|spec| spec.mode)
            .and_then(|mode| mode.replicated)
            .and_then(|replicated| replicated.replicas),
        Some(i64::from(service.replicas))
    );
    restarted
        .remove_service(&http_service.name, &labels)
        .await
        .unwrap();
    restarted
        .remove_service(&service.name, &labels)
        .await
        .unwrap();
    let removal_deadline = tokio::time::Instant::now() + Duration::from_mins(1);
    while restarted
        .observe(&app)
        .await
        .unwrap()
        .services
        .iter()
        .any(|observed| observed.name == service.name || observed.name == http_service.name)
    {
        assert!(tokio::time::Instant::now() < removal_deadline);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    restarted
        .remove_network(&network.name, &labels)
        .await
        .unwrap();
    let network_removal_deadline = tokio::time::Instant::now() + Duration::from_mins(1);
    while restarted
        .observe(&app)
        .await
        .unwrap()
        .networks
        .iter()
        .any(|observed| observed.name == network.name)
    {
        assert!(tokio::time::Instant::now() < network_removal_deadline);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let retained = restarted.observe(&app).await.unwrap();
    assert!(retained.volumes.iter().any(|v| v.name == volume.name));
    raw.remove_volume(
        &volume.name,
        None::<bollard::query_parameters::RemoveVolumeOptions>,
    )
    .await
    .unwrap();
}
