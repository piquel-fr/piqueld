//! Privileged end-to-end qualification for the Docker adapter.

#![cfg(feature = "docker-integration")]

use bollard::models::VolumeCreateOptions;
use bollard::query_parameters::{
    InspectNetworkOptions, InspectServiceOptions, UpdateServiceOptionsBuilder,
};
use futures_util::TryStreamExt as _;
use piqueld::build::{BollardBuildKit, BuildContext, BuildExecutor};
use piqueld::docker::{BollardDocker, DockerApi, DockerError};
use piqueld::proxy::{InfrastructureState, IngressApi, IngressSpec, TraefikController};
use piqueld::registry::RegistryClient;
use piqueld_core::manifest::HealthCheck;
use piqueld_core::resource::{
    Convergence, DesiredMount, DesiredNetwork, DesiredSecret, DesiredSecretMount, DesiredService,
    DesiredVolume, ResolvedSource, SECRET_LABEL,
};
use piqueld_core::{ApplicationId, InstanceId, ResourceKind, Sha256Digest, docker_resource_name};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fmt::Write as _, path::Path, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

fn fixture_image(variable: &str, default: &str) -> String {
    std::env::var(variable).unwrap_or_else(|_| default.into())
}

fn router_name(application: &ApplicationId, host: &str, service: &str, port: u16) -> String {
    let mut hasher = Sha256::new();
    hasher.update(application.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(host.as_bytes());
    hasher.update([0]);
    hasher.update(service.as_bytes());
    hasher.update(port.to_be_bytes());
    let digest = hasher.finalize();
    let mut suffix = String::with_capacity(16);
    for byte in &digest[..8] {
        write!(&mut suffix, "{byte:02x}").expect("writing to a String cannot fail");
    }
    format!("r{suffix}")
}

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

async fn wait_for_converged_replicas(
    docker: &BollardDocker,
    application: &ApplicationId,
    service_name: &str,
    replicas: u16,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_mins(2);
    loop {
        let observed = docker.observe(application).await.unwrap();
        if observed.services.iter().any(|service| {
            service.name == service_name
                && service.replicas == replicas
                && service.convergence == Convergence::Converged
        }) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "service {service_name} did not converge to {replicas} replicas: {observed:?}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_for_route(origin_port: u16) {
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let last_status = match client
            .get(format!("http://127.0.0.1:{origin_port}/"))
            .header("host", "route.test")
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => return,
            Ok(response) => Some(response.status()),
            Err(_) => None,
        };
        assert!(
            tokio::time::Instant::now() < deadline,
            "Traefik did not publish the route; last status: {last_status:?}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_for_infrastructure_absent(docker: &BollardDocker) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let client = docker.client();
        let service_absent = client
            .inspect_service("piqueld-traefik", None::<InspectServiceOptions>)
            .await
            .is_err_and(|error| {
                matches!(
                    error,
                    bollard::errors::Error::DockerResponseServerError {
                        status_code: 404,
                        ..
                    }
                )
            });
        let network_absent = client
            .inspect_network("piqueld-ingress", None::<InspectNetworkOptions>)
            .await
            .is_err_and(|error| {
                matches!(
                    error,
                    bollard::errors::Error::DockerResponseServerError {
                        status_code: 404,
                        ..
                    }
                )
            });
        if service_absent && network_absent {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Docker did not finish removing ingress infrastructure"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn remove_volume_when_released(docker: &bollard::Docker, name: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_mins(2);
    loop {
        match docker
            .remove_volume(name, None::<bollard::query_parameters::RemoveVolumeOptions>)
            .await
        {
            Ok(()) => return,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 409,
                message,
            }) if message.contains("volume is in use")
                && tokio::time::Instant::now() < deadline =>
            {
                // Swarm removes historical task containers asynchronously after
                // service deletion; those containers briefly retain the volume.
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(error) => panic!("failed to remove retained volume {name}: {error}"),
        }
    }
}

#[tokio::test]
#[ignore = "requires an isolated disposable Docker Engine and may publish a host port"]
#[allow(clippy::too_many_lines)]
async fn traefik_route_status_scale_update_and_multiplexed_logs() {
    assert_eq!(
        std::env::var("PIQUELD_DOCKER_ISOLATED").as_deref(),
        Ok("1"),
        "refusing to mutate Docker without an explicit isolated-daemon attestation"
    );
    assert_eq!(
        std::env::var("PIQUELD_DOCKER_DISPOSABLE").as_deref(),
        Ok("1"),
        "ingress qualification also requires a disposable-daemon attestation"
    );
    let socket = std::env::var("PIQUELD_DOCKER_SOCKET").expect("isolated socket required");
    let origin_port = std::env::var("PIQUELD_TEST_ORIGIN_PORT")
        .unwrap_or_else(|_| "18080".into())
        .parse()
        .expect("PIQUELD_TEST_ORIGIN_PORT must be a port");
    let docker = Arc::new(BollardDocker::connect(Path::new(&socket)).unwrap());
    docker.ensure_swarm(true).await.unwrap();
    let instance = InstanceId::parse("ingress-integration").unwrap();
    let ingress = TraefikController::new(docker.client());
    let ingress_spec = IngressSpec {
        instance_id: instance.clone(),
        image: fixture_image(
            "PIQUELD_TEST_TRAEFIK_IMAGE",
            "traefik:v3.5.0@sha256:4e7175cfe19be83c6b928cae49dde2f2788fb307189a4dc9550b67acf30c11a5",
        ),
        published_port: Some(origin_port),
        docker_socket: Path::new(&socket).to_path_buf(),
    };
    ingress.ensure(&ingress_spec).await.unwrap();
    let suffix = uuid::Uuid::now_v7().simple().to_string();
    let app = ApplicationId::parse(format!("route-{}", &suffix[..16])).unwrap();
    let labels = BTreeMap::from([
        ("io.piqueld.managed".into(), "true".into()),
        ("io.piqueld.instance".into(), instance.to_string()),
        ("io.piqueld.application".into(), app.to_string()),
        (
            "io.piqueld.spec-hash".into(),
            format!("sha256:{}", "a".repeat(64)),
        ),
    ]);
    let private = DesiredNetwork {
        name: docker_resource_name(&app, ResourceKind::Network, None),
        ingress: false,
        labels: labels.clone(),
    };
    docker.ensure_network(&private).await.unwrap();
    let nginx = fixture_image("PIQUELD_TEST_NGINX_IMAGE", "nginx:1.27-alpine");
    let image = docker.resolve_image(&nginx).await.unwrap();
    let router = router_name(&app, "route.test", "web", 80);
    let backend = format!("{router}-backend");
    let mut service_labels = labels.clone();
    service_labels.extend([
        ("io.piqueld.service".into(), "web".into()),
        ("traefik.enable".into(), "true".into()),
        ("traefik.swarm.network".into(), "piqueld-ingress".into()),
        (
            format!("traefik.http.routers.{router}.rule"),
            "Host(`route.test`)".into(),
        ),
        (
            format!("traefik.http.routers.{router}.entrypoints"),
            "web".into(),
        ),
        (
            format!("traefik.http.routers.{router}.service"),
            backend.clone(),
        ),
        (
            format!("traefik.http.services.{backend}.loadbalancer.server.port"),
            "80".into(),
        ),
    ]);
    let mut service = DesiredService {
        logical_name: "web".into(),
        name: docker_resource_name(&app, ResourceKind::Service, Some("web")),
        source: ResolvedSource::Image {
            requested: nginx,
            digest_reference: image.clone(),
        },
        image,
        replicas: 1,
        environment: BTreeMap::new(),
        command: vec![],
        arguments: vec![],
        ports: vec![80],
        mounts: vec![],
        secrets: vec![],
        healthcheck: None,
        resources: None,
        networks: vec![private.name.clone(), "piqueld-ingress".into()],
        labels: service_labels,
    };
    docker.ensure_service(&service).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_mins(2);
    loop {
        let app_ready = docker
            .observe(&app)
            .await
            .unwrap()
            .services
            .iter()
            .any(|seen| seen.convergence == Convergence::Converged);
        let proxy_ready =
            ingress.status(&ingress_spec).await.unwrap() == InfrastructureState::Ready;
        if app_ready && proxy_ready {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "route did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    wait_for_route(origin_port).await;
    service.replicas = 2;
    docker.ensure_service(&service).await.unwrap();
    wait_for_converged_replicas(&docker, &app, &service.name, 2).await;
    wait_for_route(origin_port).await;
    let logs = docker
        .application_logs(
            &instance,
            &app,
            &piqueld::docker::RuntimeLogQuery {
                since_seconds: 0,
                tail: 100,
                max_bytes: 256 * 1024,
            },
        )
        .await
        .unwrap();
    assert!(
        !logs.is_empty()
            && logs
                .iter()
                .all(|record| record.service == "web" && !record.task_id.is_empty())
    );
    docker.remove_service(&service.name, &labels).await.unwrap();
    docker.remove_network(&private.name, &labels).await.unwrap();
    docker
        .client()
        .delete_service("piqueld-traefik")
        .await
        .unwrap();
    docker
        .client()
        .remove_network("piqueld-ingress")
        .await
        .unwrap();
    wait_for_infrastructure_absent(&docker).await;
    ingress.ensure(&ingress_spec).await.unwrap();
}

#[tokio::test]
#[ignore = "requires an isolated Docker Engine with BuildKit and a disposable insecure registry"]
#[allow(clippy::too_many_lines)]
async fn buildkit_push_registry_digest_and_swarm_deploy() {
    assert_eq!(std::env::var("PIQUELD_DOCKER_ISOLATED").as_deref(), Ok("1"));
    assert_eq!(
        std::env::var("PIQUELD_DOCKER_DISPOSABLE").as_deref(),
        Ok("1")
    );
    let socket = std::env::var("PIQUELD_DOCKER_SOCKET").expect("isolated socket required");
    let registry = std::env::var("PIQUELD_TEST_REGISTRY").expect("registry authority required");
    let docker = BollardDocker::connect(Path::new(&socket)).unwrap();
    docker.ensure_swarm(true).await.unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let alpine = fixture_image("PIQUELD_TEST_ALPINE_IMAGE", "alpine:3.20");
    std::fs::write(
        fixture.path().join("Dockerfile"),
        format!("FROM {alpine}\nCMD [\"/bin/sh\",\"-c\",\"while true; do sleep 5; done\"]\n"),
    )
    .unwrap();
    let context = BuildContext::create(fixture.path(), ".", "Dockerfile", 1024 * 1024).unwrap();
    let suffix = uuid::Uuid::now_v7().simple().to_string();
    let repository = format!("integration/{}/web", &suffix[..12]);
    let tag = format!("{registry}/{repository}:fixture");
    let backend = Arc::new(BollardBuildKit::new(docker.client()));
    let executor = BuildExecutor::new(backend, 1, Duration::from_mins(5));
    executor
        .execute(context, &tag, None, CancellationToken::new())
        .await
        .unwrap();
    let registry_client =
        RegistryClient::new(&format!("http://{registry}"), Duration::from_secs(15)).unwrap();
    let digest = registry_client
        .verified_reference(&repository, "fixture")
        .await
        .unwrap();
    let app = ApplicationId::parse(format!("build-{}", &suffix[..16])).unwrap();
    let instance = InstanceId::parse("build-integration").unwrap();
    let labels = BTreeMap::from([
        ("io.piqueld.managed".into(), "true".into()),
        ("io.piqueld.instance".into(), instance.to_string()),
        ("io.piqueld.application".into(), app.to_string()),
        (
            "io.piqueld.spec-hash".into(),
            format!("sha256:{}", "a".repeat(64)),
        ),
        ("io.piqueld.service".into(), "web".into()),
    ]);
    let service = DesiredService {
        logical_name: "web".into(),
        name: docker_resource_name(&app, ResourceKind::Service, Some("web")),
        source: ResolvedSource::Git {
            repository: "https://example.invalid/repo".into(),
            requested_reference: "main".into(),
            commit: "a".repeat(40),
            context: ".".into(),
            dockerfile: "Dockerfile".into(),
            registry_reference: tag,
            digest_reference: digest.clone(),
        },
        image: digest,
        replicas: 1,
        environment: BTreeMap::new(),
        command: vec![],
        arguments: vec![],
        ports: vec![],
        mounts: vec![],
        secrets: vec![],
        healthcheck: None,
        resources: None,
        networks: vec![],
        labels,
    };
    docker.ensure_service(&service).await.unwrap();
    wait_for_converged_replicas(&docker, &app, &service.name, 1).await;
    assert_eq!(
        docker
            .observe(&app)
            .await
            .unwrap()
            .services
            .iter()
            .find(|seen| seen.name == service.name)
            .unwrap()
            .image,
        service.image
    );
    let ownership = BTreeMap::from([
        ("io.piqueld.managed".into(), "true".into()),
        ("io.piqueld.instance".into(), instance.to_string()),
        ("io.piqueld.application".into(), app.to_string()),
        (
            "io.piqueld.spec-hash".into(),
            format!("sha256:{}", "a".repeat(64)),
        ),
    ]);
    docker
        .remove_service(&service.name, &ownership)
        .await
        .unwrap();
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
    assert_eq!(
        std::env::var("PIQUELD_DOCKER_DISPOSABLE").as_deref(),
        Ok("1"),
        "lifecycle qualification requires a disposable-daemon attestation"
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
        ingress: false,
        labels: labels.clone(),
    };
    let volume = DesiredVolume {
        logical_name: "data".into(),
        name: docker_resource_name(&app, ResourceKind::Volume, Some("data")),
        labels: labels.clone(),
    };
    docker.ensure_network(&network).await.unwrap();
    docker.ensure_volume(&volume).await.unwrap();
    let alpine = fixture_image("PIQUELD_TEST_ALPINE_IMAGE", "alpine:3.20");
    let image = docker.resolve_image(&alpine).await.unwrap();
    let mut application_hash = String::with_capacity(10);
    for byte in &Sha256::digest(app.as_str().as_bytes())[..5] {
        use std::fmt::Write as _;
        write!(&mut application_hash, "{byte:02x}").unwrap();
    }
    let secret_name = format!(
        "piqueld-secret-api-token-{}-{application_hash}",
        "a".repeat(22)
    );
    let mut secret_labels = labels.clone();
    secret_labels.insert(SECRET_LABEL.into(), "api-token".into());
    let secret = DesiredSecret {
        logical_name: "api-token".into(),
        generation: Sha256Digest::parse(format!("sha256:{}", "a".repeat(64))).unwrap(),
        name: secret_name.clone(),
        labels: secret_labels,
    };
    docker
        .ensure_secret(&secret, b"docker-qualification-canary")
        .await
        .unwrap();
    let mut service_labels = labels.clone();
    service_labels.insert("io.piqueld.service".into(), "web".into());
    let mut service = DesiredService {
        logical_name: "web".into(),
        name: docker_resource_name(&app, ResourceKind::Service, Some("web")),
        source: ResolvedSource::Image {
            requested: alpine,
            digest_reference: image.clone(),
        },
        image: image.clone(),
        replicas: 1,
        environment: BTreeMap::new(),
        command: vec!["/bin/sh".into()],
        arguments: vec![
            "-c".into(),
            "test \"$(cat /run/secrets/api-token)\" = docker-qualification-canary && printf volume-retained > /data/marker && while true; do echo qualification-log; sleep 5; done".into(),
        ],
        ports: vec![],
        mounts: vec![DesiredMount {
            volume_name: volume.name.clone(),
            target: "/data".into(),
            read_only: false,
        }],
        secrets: vec![DesiredSecretMount {
            logical_name: "api-token".into(),
            swarm_name: secret_name,
            target: "/run/secrets/api-token".into(),
            mode: "0400".into(),
        }],
        healthcheck: Some(HealthCheck::Command {
            command: vec!["/bin/sh".into(), "-c".into(), "test -s /data/marker".into()],
            interval_seconds: 2,
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

    wait_for_converged_replicas(&docker, &app, &service.name, 1).await;
    service.replicas = 2;
    ensure_service_eventually(&docker, &service).await;
    // Reconnecting exercises the same observation/recovery seam used after daemon restart.
    let restarted = BollardDocker::connect(Path::new(&socket)).unwrap();
    wait_for_converged_replicas(&restarted, &app, &service.name, 2).await;
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
    wait_for_converged_replicas(&restarted, &app, &service.name, 1).await;
    restarted.ensure_service(&service).await.unwrap();
    wait_for_converged_replicas(&restarted, &app, &service.name, 2).await;

    service.healthcheck = Some(HealthCheck::Command {
        command: vec!["/bin/sh".into(), "-c".into(), "exit 1".into()],
        interval_seconds: 1,
        timeout_seconds: 1,
    });
    restarted.ensure_service(&service).await.unwrap();
    let health_deadline = tokio::time::Instant::now() + Duration::from_mins(2);
    loop {
        let observed = restarted.observe(&app).await.unwrap();
        if observed.services.iter().any(|seen| {
            seen.name == service.name
                && matches!(
                    seen.convergence,
                    Convergence::Degraded | Convergence::Failed
                )
        }) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < health_deadline,
            "failing health check was falsely treated as converged: {observed:?}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let foreign_app = ApplicationId::parse(format!("foreign-{}", &suffix[16..28])).unwrap();
    let foreign_volume = DesiredVolume {
        logical_name: "data".into(),
        name: docker_resource_name(&foreign_app, ResourceKind::Volume, Some("data")),
        labels: BTreeMap::from([
            ("io.piqueld.managed".into(), "true".into()),
            ("io.piqueld.instance".into(), instance.to_string()),
            ("io.piqueld.application".into(), foreign_app.to_string()),
            (
                "io.piqueld.spec-hash".into(),
                format!("sha256:{}", "b".repeat(64)),
            ),
        ]),
    };
    raw.create_volume(VolumeCreateOptions {
        name: Some(foreign_volume.name.clone()),
        driver: Some("local".into()),
        driver_opts: Some(std::collections::HashMap::new()),
        labels: Some(std::collections::HashMap::new()),
        ..Default::default()
    })
    .await
    .unwrap();
    assert!(matches!(
        restarted.ensure_volume(&foreign_volume).await,
        Err(DockerError::OwnershipConflict)
    ));
    raw.remove_volume(
        &foreign_volume.name,
        None::<bollard::query_parameters::RemoveVolumeOptions>,
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
    let verifier_name = format!("piqueld-volume-verifier-{}", &suffix[..12]);
    raw.create_container(
        Some(bollard::query_parameters::CreateContainerOptions {
            name: Some(verifier_name.clone()),
            ..Default::default()
        }),
        bollard::models::ContainerCreateBody {
            image: Some(image),
            cmd: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                "test \"$(cat /data/marker)\" = volume-retained".into(),
            ]),
            host_config: Some(bollard::models::HostConfig {
                mounts: Some(vec![bollard::models::Mount {
                    target: Some("/data".into()),
                    source: Some(volume.name.clone()),
                    typ: Some(bollard::models::MountTypeEnum::VOLUME),
                    read_only: Some(true),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    raw.start_container(
        &verifier_name,
        None::<bollard::query_parameters::StartContainerOptions>,
    )
    .await
    .unwrap();
    let result = raw
        .wait_container(
            &verifier_name,
            None::<bollard::query_parameters::WaitContainerOptions>,
        )
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(result.first().unwrap().status_code, 0);
    raw.remove_container(
        &verifier_name,
        Some(bollard::query_parameters::RemoveContainerOptions {
            force: true,
            ..Default::default()
        }),
    )
    .await
    .unwrap();
    restarted
        .remove_secret(&secret.name, &labels)
        .await
        .unwrap();
    remove_volume_when_released(&raw, &volume.name).await;
}
