//! Privileged end-to-end qualification for the Docker adapter.

use bollard::query_parameters::{InspectServiceOptions, UpdateServiceOptionsBuilder};
#[path = "support/git.rs"]
mod git_fixture;
use git_fixture::GitBuildFixture;
use piqueld::application::RuntimeBoundary;
use piqueld::docker::{BollardDocker, DockerApi, DockerError};
use piqueld_core::manifest::HealthCheck;
use piqueld_core::resource::{DesiredNetwork, DesiredService, DesiredVolume, ResolvedSource};
use piqueld_core::{ApplicationId, InstanceId, ResourceKind, docker_resource_name};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};

struct IsolatedDocker {
    socket: PathBuf,
    docker: BollardDocker,
}

impl IsolatedDocker {
    async fn from_env() -> Self {
        assert_eq!(
            std::env::var("PIQUELD_DOCKER_ISOLATED").as_deref(),
            Ok("1"),
            "refusing to mutate Docker without an explicit isolated-daemon attestation"
        );
        let socket = std::env::var("PIQUELD_DOCKER_SOCKET")
            .expect("PIQUELD_DOCKER_SOCKET must point at an isolated privileged daemon");
        // Defense in depth: the default host socket is refused even when the
        // isolation attestation variable is set. Canonicalize first, because
        // /var/run is normally a symlink to /run and the configured value may be
        // relative.
        let resolved = std::fs::canonicalize(&socket).unwrap_or_else(|_| PathBuf::from(&socket));
        for forbidden in ["/var/run/docker.sock", "/run/docker.sock"] {
            let forbidden =
                std::fs::canonicalize(forbidden).unwrap_or_else(|_| PathBuf::from(forbidden));
            assert_ne!(
                resolved, forbidden,
                "refusing to mutate the default host Docker socket"
            );
        }
        let docker = BollardDocker::connect(&resolved).unwrap();
        docker.ensure_swarm(true).await.unwrap();
        Self {
            socket: resolved,
            docker,
        }
    }

    async fn ensure_service_eventually(&self, desired: &DesiredService) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            match self.docker.ensure_service(desired).await {
                Ok(()) => return,
                Err(DockerError::Request(_)) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => panic!("failed to ensure Docker service: {error}"),
            }
        }
    }
}

struct SwarmScenario {
    engine: IsolatedDocker,
    app: ApplicationId,
    labels: BTreeMap<String, String>,
    network: DesiredNetwork,
    volume: DesiredVolume,
    service: DesiredService,
}

impl SwarmScenario {
    async fn new() -> Self {
        let engine = IsolatedDocker::from_env().await;
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
            name: piqueld_core::DockerNetworkName::parse(docker_resource_name(
                &app,
                ResourceKind::Network,
                None,
            ))
            .unwrap(),
            labels: labels.clone(),
        };
        let volume = DesiredVolume {
            logical_name: piqueld_core::VolumeName::parse("data").unwrap(),
            name: piqueld_core::DockerVolumeName::parse(docker_resource_name(
                &app,
                ResourceKind::Volume,
                Some("data"),
            ))
            .unwrap(),
            labels: labels.clone(),
        };
        engine.docker.ensure_network(&network).await.unwrap();
        let observed = engine.docker.observe(&app).await.unwrap();
        assert!(
            observed.networks[0].runtime_configuration_matches,
            "a network created by piqueld must match Docker's inspected representation"
        );
        engine.docker.ensure_volume(&volume).await.unwrap();
        let image = engine.docker.resolve_image("alpine:3.20").await.unwrap();
        let mut service_labels = labels.clone();
        service_labels.insert("io.piqueld.service".into(), "web".into());
        let service = DesiredService {
            logical_name: piqueld_core::ServiceName::parse("web").unwrap(),
            name: piqueld_core::DockerServiceName::parse(docker_resource_name(
                &app,
                ResourceKind::Service,
                Some("web"),
            ))
            .unwrap(),
            source: ResolvedSource::parse_image("alpine:3.20", image.clone()).unwrap(),
            image: piqueld_core::ImmutableImage::parse(image).unwrap(),
            replicas: 1,
            environment: BTreeMap::new(),
            command: vec!["/bin/sh".into()],
            arguments: vec![
                "-c".into(),
                "echo log-stdout; echo log-stderr >&2; while true; do sleep 5; done".into(),
            ],
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
        engine.ensure_service_eventually(&service).await;

        Self {
            engine,
            app,
            labels,
            network,
            volume,
            service,
        }
    }

    async fn assert_logs(&self) {
        let instance = InstanceId::parse(&self.labels["io.piqueld.instance"]).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let logs = self
                .engine
                .docker
                .application_logs(&instance, &self.app, Some("web"), 20, 60)
                .await
                .unwrap();
            if logs.items.iter().any(|line| line.message == "log-stderr") {
                assert!(
                    logs.items
                        .iter()
                        .any(|line| line.message == "log-stdout" && line.stream == "stdout")
                );
                assert!(logs.items.iter().all(|line| line.service == "web"
                    && !line.task_id.is_empty()
                    && !line.timestamp.is_empty()));
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "container output did not appear"
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert!(
            self.engine
                .docker
                .application_logs(
                    &InstanceId::parse("another-instance").unwrap(),
                    &self.app,
                    None,
                    20,
                    60
                )
                .await
                .unwrap()
                .items
                .is_empty()
        );
    }

    async fn add_http_service(&self) -> DesiredService {
        let mut http_service = self.service.clone();
        http_service.logical_name = piqueld_core::ServiceName::parse("http").unwrap();
        http_service.name = piqueld_core::DockerServiceName::parse(docker_resource_name(
            &self.app,
            ResourceKind::Service,
            Some("http"),
        ))
        .unwrap();
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
        self.engine.ensure_service_eventually(&http_service).await;

        http_service
    }

    async fn assert_healthchecks(&self, http_service: &DesiredService) {
        let observed = self.engine.docker.observe(&self.app).await.unwrap();
        assert_eq!(
            observed
                .services
                .iter()
                .find(|candidate| candidate.name == self.service.name.as_str())
                .and_then(|candidate| candidate.healthcheck.as_ref()),
            self.service.healthcheck.as_ref(),
            "command health check survives complete service inspection"
        );
        assert_eq!(
            observed
                .services
                .iter()
                .find(|candidate| candidate.name == http_service.name.as_str())
                .and_then(|candidate| candidate.healthcheck.as_ref()),
            http_service.healthcheck.as_ref(),
            "HTTP health check survives complete service inspection"
        );

        // Tasks of health-checked services must surface the live container
        // healthcheck verdict once the probe has run at least once.
        let health_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let observed = self.engine.docker.observe(&self.app).await.unwrap();
            let healthy = observed
                .services
                .iter()
                .find(|candidate| candidate.name == self.service.name.as_str())
                .and_then(|candidate| candidate.tasks.iter().find(|task| task.desired_running))
                .map(|task| task.healthy);
            if healthy == Some(Some(true)) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < health_deadline,
                "the command health check never reported a healthy task: {healthy:?}"
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn scale_and_reconnect(&mut self) {
        self.service.replicas = 2;
        self.engine.ensure_service_eventually(&self.service).await;
        // Reconnecting exercises the same observation/recovery seam used after daemon restart.
        self.engine.docker = BollardDocker::connect(&self.engine.socket).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_mins(1);
        loop {
            let observed = self.engine.docker.observe(&self.app).await.unwrap();
            if observed
                .services
                .iter()
                .any(|s| s.name == self.service.name.as_str() && s.replicas == 2)
            {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let observed = self.engine.docker.observe(&self.app).await.unwrap();
        assert_eq!(
            observed
                .services
                .iter()
                .find(|candidate| candidate.name == self.service.name.as_str())
                .and_then(|candidate| candidate.healthcheck.as_ref()),
            self.service.healthcheck.as_ref()
        );
    }

    async fn assert_idempotence_and_repair_drift(&self) {
        let raw = bollard::Docker::connect_with_unix(
            self.engine.socket.to_str().unwrap(),
            120,
            bollard::API_DEFAULT_VERSION,
        )
        .unwrap();
        let matching = raw
            .inspect_service(self.service.name.as_str(), None::<InspectServiceOptions>)
            .await
            .unwrap();
        let matching_version = matching.version.as_ref().and_then(|version| version.index);
        self.engine.ensure_service_eventually(&self.service).await;
        let unchanged = raw
            .inspect_service(self.service.name.as_str(), None::<InspectServiceOptions>)
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
            self.service.name.as_str(),
            drifted_spec,
            UpdateServiceOptionsBuilder::default()
                .version(i32::try_from(matching_version.unwrap()).unwrap())
                .build(),
            None,
        )
        .await
        .unwrap();
        self.engine
            .docker
            .ensure_service(&self.service)
            .await
            .unwrap();
        let repaired = raw
            .inspect_service(self.service.name.as_str(), None::<InspectServiceOptions>)
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
            Some(i64::from(self.service.replicas))
        );
    }

    async fn delete_retaining_volume(&self, http_service: &DesiredService) {
        self.engine
            .docker
            .remove_service(http_service.name.as_str(), &self.labels)
            .await
            .unwrap();
        self.engine
            .docker
            .remove_service(self.service.name.as_str(), &self.labels)
            .await
            .unwrap();
        let removal_deadline = tokio::time::Instant::now() + Duration::from_mins(1);
        while self
            .engine
            .docker
            .observe(&self.app)
            .await
            .unwrap()
            .services
            .iter()
            .any(|observed| {
                observed.name == self.service.name.as_str()
                    || observed.name == http_service.name.as_str()
            })
        {
            assert!(tokio::time::Instant::now() < removal_deadline);
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        self.engine
            .docker
            .remove_network(self.network.name.as_str(), &self.labels)
            .await
            .unwrap();
        let network_removal_deadline = tokio::time::Instant::now() + Duration::from_mins(1);
        while self
            .engine
            .docker
            .observe(&self.app)
            .await
            .unwrap()
            .networks
            .iter()
            .any(|observed| observed.name == self.network.name.as_str())
        {
            assert!(tokio::time::Instant::now() < network_removal_deadline);
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let retained = self.engine.docker.observe(&self.app).await.unwrap();
        assert!(
            retained
                .volumes
                .iter()
                .any(|v| v.name == self.volume.name.as_str())
        );
        let raw = bollard::Docker::connect_with_unix(
            self.engine.socket.to_str().unwrap(),
            120,
            bollard::API_DEFAULT_VERSION,
        )
        .unwrap();
        raw.remove_volume(
            self.volume.name.as_str(),
            None::<bollard::query_parameters::RemoveVolumeOptions>,
        )
        .await
        .unwrap();
    }
}

/// Privileged lifecycle qualification; ordinary validation never mutates Docker.
#[tokio::test]
#[ignore = "requires an isolated privileged Docker Engine"]
async fn swarm_init_create_replica_drift_restart_delete_and_volume_retention() {
    let mut scenario = SwarmScenario::new().await;
    scenario.assert_logs().await;
    let http_service = scenario.add_http_service().await;
    scenario.assert_healthchecks(&http_service).await;
    scenario.scale_and_reconnect().await;
    scenario.assert_idempotence_and_repair_drift().await;
    scenario.delete_retaining_volume(&http_service).await;
}

#[tokio::test]
#[ignore = "requires an isolated privileged Docker Engine"]
async fn git_build_runs_as_a_local_swarm_image() {
    let engine = IsolatedDocker::from_env().await;
    let docker = &engine.docker;
    let fixture = GitBuildFixture::new();
    let source = fixture.source.clone();
    let manifest = serde_json::json!({"api_version":"piqueld.dev/v1alpha1", "kind":"Application", "metadata":{"name":"git-local"}, "spec":{"services":[{"name":"web", "source":source}]}});
    let app = piqueld_core::parse_json(&manifest.to_string())
        .unwrap()
        .normalize(ApplicationId::parse("git-local-build").unwrap());
    let runtime = piqueld::application::DockerRuntime::new(
        std::sync::Arc::new(docker.clone()),
        InstanceId::parse("git-build-test").unwrap(),
        std::sync::Arc::new(tokio::sync::Notify::new()),
        Duration::from_secs(120),
    );
    let target = runtime
        .prepare(&app, &piqueld_core::ResolutionSet::default())
        .await
        .unwrap();
    docker.ensure_network(&target.networks[0]).await.unwrap();
    let observed = docker.observe(app.id()).await.unwrap();
    assert!(observed.networks[0].runtime_configuration_matches);
    docker.ensure_network(&target.networks[0]).await.unwrap();
    engine.ensure_service_eventually(&target.services[0]).await;
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let observed = docker.observe(app.id()).await.unwrap();
            if observed
                .services
                .iter()
                .any(|service| service.convergence == piqueld_core::Convergence::Converged)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .expect("local Git-built image must converge in Swarm");
    docker
        .remove_service(target.services[0].name.as_str(), &target.services[0].labels)
        .await
        .unwrap();
}
