//! Privileged end-to-end qualification for the Docker adapter.

use bollard::query_parameters::{InspectServiceOptions, UpdateServiceOptionsBuilder};
#[path = "support/git.rs"]
mod git_fixture;
use git_fixture::GitBuildFixture;
use piqueld::application::RuntimeBoundary;
use piqueld::docker::{BollardDocker, DockerApi, DockerError, JobRuns, JobStatus};
use piqueld_core::manifest::ValidatedHealthCheck;
use piqueld_core::resource::{DesiredNetwork, DesiredService, DesiredVolume, ResolvedSource};
use piqueld_core::{ApplicationId, EnvironmentId, InstanceId, ResourceKind, docker_resource_name};
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
    app: EnvironmentId,
    labels: BTreeMap<String, String>,
    network: DesiredNetwork,
    volume: DesiredVolume,
    service: DesiredService,
}

impl SwarmScenario {
    async fn new() -> Self {
        let engine = IsolatedDocker::from_env().await;
        let suffix = uuid::Uuid::now_v7().simple().to_string();
        let app = EnvironmentId::parse(format!("app-{}", &suffix[..16])).unwrap();
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
        let secret_name = format!("piqueld-secret-{suffix}");
        engine
            .docker
            .ensure_secret(&secret_name, b"mounted-value", &labels)
            .await
            .unwrap();
        let service = DesiredService {
            secrets: vec![piqueld_core::resource::SecretFile { secret_name: secret_name.clone(), target: "/run/secrets/token".into() }],
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
                "test $(cat /run/secrets/token) = mounted-value || exit 1; until getent hosts web >/dev/null; do sleep 1; done; echo log-stdout; echo log-stderr >&2; while true; do sleep 5; done".into(),
            ],
            mounts: vec![],
            healthcheck: Some(ValidatedHealthCheck::Command {
                command: vec!["true".into()],
                interval_seconds: 1,
                timeout_seconds: 1,
            }),
            resources: None,
            networks: vec![network.name.clone()],
            labels: service_labels,
            depends_on: Vec::new(),
            rollout: piqueld_core::manifest::ValidatedRollout::default(),
        };
        engine.ensure_service_eventually(&service).await;
        assert!(
            engine
                .docker
                .remove_secrets(std::slice::from_ref(&secret_name), &labels)
                .await
                .is_err(),
            "Docker refuses removal while a service uses the secret"
        );

        Self {
            engine,
            app,
            labels,
            network,
            volume,
            service,
        }
    }

    /// The container logs only after resolving its logical name, so this also
    /// proves the private-network alias.
    async fn assert_logs(&self) {
        let instance = InstanceId::parse(&self.labels["io.piqueld.instance"]).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let logs = self
                .engine
                .docker
                .application_logs(&instance, &self.app, Some("web"), 20, 60, None)
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
        for stream in [
            piqueld_core::api::LogStream::Stdout,
            piqueld_core::api::LogStream::Stderr,
        ] {
            let logs = self
                .engine
                .docker
                .application_logs(&instance, &self.app, Some("web"), 1, 60, Some(stream))
                .await
                .unwrap();
            assert_eq!(logs.items.len(), 1);
            assert_eq!(logs.items[0].stream, stream.as_str());
        }
        assert!(
            self.engine
                .docker
                .application_logs(
                    &InstanceId::parse("another-instance").unwrap(),
                    &self.app,
                    None,
                    20,
                    60,
                    None
                )
                .await
                .unwrap()
                .items
                .is_empty()
        );
    }

    /// Runs `script` in the web task, sending `stdin`, and returns output and exit code.
    /// Creates `script` as a `/bin/sh -c` command in the running `web` task.
    async fn create_exec(
        &self,
        script: &str,
        tty: Option<piqueld_core::exec::TerminalSize>,
    ) -> piqueld::docker::Exec {
        use piqueld_core::exec::{ExecCommand, ExecRequest};
        let instance = InstanceId::parse(&self.labels["io.piqueld.instance"]).unwrap();
        let request = ExecRequest {
            service: piqueld_core::ServiceName::parse("web").unwrap(),
            command: ExecCommand::parse(vec!["/bin/sh".into(), "-c".into(), script.into()])
                .unwrap(),
            stdin: true,
            tty,
        };
        self.engine
            .docker
            .create_exec(&instance, &self.app, &request)
            .await
            .unwrap()
            .expect("web has a running task")
    }

    /// Runs `script` with `stdin` and returns its stdout, stderr and exit code.
    async fn exec(
        &self,
        script: &str,
        stdin: &[u8],
        tty: Option<piqueld_core::exec::TerminalSize>,
    ) -> (Vec<u8>, Vec<u8>, i64) {
        use piqueld_core::exec::{ExecInput, ExecOutput};
        let exec = self.create_exec(script, tty).await;
        let (input, input_rx) = tokio::sync::mpsc::channel(4);
        let (output_tx, mut output) = tokio::sync::mpsc::channel(16);
        // Dropping `_connected` before the command exits would disconnect.
        let (_connected, disconnected) = tokio::sync::oneshot::channel();
        input.send(ExecInput::Stdin(stdin.to_vec())).await.unwrap();
        input.send(ExecInput::CloseStdin).await.unwrap();
        let io = piqueld::docker::ExecIo {
            input: input_rx,
            output: output_tx,
            disconnected,
        };
        let drain = async {
            let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
            while let Some(frame) = output.recv().await {
                match frame {
                    ExecOutput::Stdout(data) => stdout.extend(data),
                    ExecOutput::Stderr(data) => stderr.extend(data),
                    frame => panic!("unexpected exec output {frame:?}"),
                }
            }
            (stdout, stderr)
        };
        let (code, (stdout, stderr)) = tokio::time::timeout(
            Duration::from_secs(60),
            futures_util::future::join(self.engine.docker.run_exec(&exec, io), drain),
        )
        .await
        .expect("exec finishes");
        (stdout, stderr, code.unwrap())
    }

    async fn assert_exec(&self) {
        let (stdout, stderr, code) = self.exec("cat; echo failed >&2; exit 3", b"in", None).await;
        assert_eq!(
            (stdout.as_slice(), stderr.as_slice(), code),
            (&b"in"[..], &b"failed\n"[..], 3)
        );
        let size = piqueld_core::exec::TerminalSize {
            width: 91,
            height: 17,
        };
        let (stdout, stderr, code) = self.exec("stty size", b"", Some(size)).await;
        assert_eq!(String::from_utf8(stdout).unwrap().trim(), "17 91");
        assert_eq!(stderr, b"");
        assert_eq!(code, 0);
        // Unread input must not stop output from draining.
        let (stdout, _, code) = self
            .exec("head -c 8388608 /dev/zero", &vec![0; 8 << 20], None)
            .await;
        assert_eq!((stdout.len(), code), (8 << 20, 0));
        // A disconnected client ends a silent session, even while its unread
        // input blocks the command's standard input.
        let exec = self.create_exec("sleep 300", None).await;
        let (input, input_rx) = tokio::sync::mpsc::channel(1);
        let (output, _output_rx) = tokio::sync::mpsc::channel(1);
        input
            .send(piqueld_core::exec::ExecInput::Stdin(vec![0; 8 << 20]))
            .await
            .unwrap();
        let (connected, disconnected) = tokio::sync::oneshot::channel();
        let io = piqueld::docker::ExecIo {
            input: input_rx,
            output,
            disconnected,
        };
        // Disconnect only once the 8 MiB write is blocked on the unread pipe.
        let disconnect = async {
            tokio::time::sleep(Duration::from_secs(2)).await;
            drop(connected);
        };
        let (stopped, ()) = tokio::time::timeout(
            Duration::from_secs(30),
            futures_util::future::join(self.engine.docker.run_exec(&exec, io), disconnect),
        )
        .await
        .expect("a disconnect stops the session");
        assert!(stopped.is_err());
        let instance = InstanceId::parse(&self.labels["io.piqueld.instance"]).unwrap();
        let mut missing = piqueld_core::exec::ExecRequest {
            service: piqueld_core::ServiceName::parse("absent").unwrap(),
            command: piqueld_core::exec::ExecCommand::parse(vec!["true".into()]).unwrap(),
            stdin: false,
            tty: None,
        };
        let docker = &self.engine.docker;
        assert!(
            docker
                .create_exec(&instance, &self.app, &missing)
                .await
                .unwrap()
                .is_none()
        );
        missing.service = piqueld_core::ServiceName::parse("web").unwrap();
        let other = InstanceId::parse("another-instance").unwrap();
        assert!(
            docker
                .create_exec(&other, &self.app, &missing)
                .await
                .unwrap()
                .is_none()
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
        http_service.healthcheck = Some(ValidatedHealthCheck::Http {
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
                .find(|s| s.name == self.service.name.as_str())
                .unwrap()
                .secrets,
            self.service.secrets
        );
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
        let node_id = raw.info().await.unwrap().swarm.unwrap().node_id.unwrap();
        let expected_constraint = vec![format!("node.id == {node_id}")];
        assert_eq!(
            matching
                .spec
                .as_ref()
                .unwrap()
                .task_template
                .as_ref()
                .unwrap()
                .placement
                .as_ref()
                .unwrap()
                .constraints
                .as_ref(),
            Some(&expected_constraint)
        );
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
        drifted_spec.task_template.as_mut().unwrap().placement = None;
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
                .as_ref()
                .unwrap()
                .task_template
                .as_ref()
                .unwrap()
                .placement
                .as_ref()
                .unwrap()
                .constraints
                .as_ref(),
            Some(&expected_constraint)
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

    /// The `migrate` job derived from the service, running `script`.
    fn job(&self, operation: &str, script: &str) -> piqueld_core::DesiredJobRun {
        let name = piqueld_core::JobName::parse("migrate").unwrap();
        let mut container = self.service.clone();
        container.name = piqueld_core::DockerServiceName::for_job(&self.app, &name);
        container.command = vec!["/bin/sh".into(), "-c".into(), script.into()];
        container.arguments.clear();
        container.healthcheck = None;
        container.labels = self.labels.clone();
        container
            .labels
            .insert(piqueld_core::resource::JOB_LABEL.into(), name.to_string());
        piqueld_core::DesiredJob {
            logical_name: name,
            run: piqueld_core::manifest::JobRun::BeforeRollout,
            timeout_seconds: 60,
            container,
        }
        .for_operation(operation)
    }

    /// Runs one-shot jobs derived from the service: they read its secret,
    /// never answer for its alias, ignore image health checks, report exit
    /// codes and output, and are replaced or removed by operation.
    async fn run_jobs(&self) {
        let docker = &self.engine.docker;
        let first = self.job(
            "operation-1",
            "test \"$(cat /run/secrets/token)\" = mounted-value && echo out && echo err >&2",
        );
        assert_eq!(docker.job_status(&first).await.unwrap(), JobStatus::Missing);
        docker.start_job(&first).await.unwrap();
        // Bollard's typed model misses Docker's `Healthcheck` key, so the
        // stored specification is read as raw JSON.
        let spec = self
            .docker_api("GET", &format!("/services/{}", first.container.name))
            .await["Spec"]
            .take();
        assert!(spec["Mode"]["ReplicatedJob"].is_object(), "{spec}");
        let task = &spec["TaskTemplate"];
        assert_eq!(
            task["ContainerSpec"]["Healthcheck"]["Test"],
            serde_json::json!(["NONE"])
        );
        assert_eq!(task["RestartPolicy"]["Condition"], "none");
        let networks = task["Networks"].as_array().unwrap();
        assert!(
            !networks.is_empty()
                && networks
                    .iter()
                    .all(|network| network["Aliases"].as_array().is_none_or(Vec::is_empty)),
            "a job must not answer for the service's alias: {spec}"
        );
        assert_eq!(
            Self::wait_job(docker, &first).await,
            JobStatus::Finished {
                exit_code: Some(0),
                error: None
            }
        );
        // Docker copies stdout and stderr independently, so their relative
        // order is not guaranteed.
        let mut output = docker.job_output(&first).await.unwrap();
        assert!(!output.truncated);
        output
            .chunks
            .sort_by_key(|(stream, _)| *stream == piqueld_core::api::LogStream::Stderr);
        assert_eq!(
            output.chunks,
            [
                (piqueld_core::api::LogStream::Stdout, b"out\n".to_vec()),
                (piqueld_core::api::LogStream::Stderr, b"err\n".to_vec()),
            ]
        );

        // Another operation's run of the same job replaces this one.
        let second = self.job("operation-2", "exit 3");
        assert_eq!(
            docker.job_status(&second).await.unwrap(),
            JobStatus::Missing
        );
        docker.start_job(&second).await.unwrap();
        assert_eq!(docker.job_status(&first).await.unwrap(), JobStatus::Missing);
        let JobStatus::Finished { exit_code, error } = Self::wait_job(docker, &second).await else {
            panic!("the failing job must finish");
        };
        assert_eq!(exit_code, Some(3));
        assert!(error.is_some(), "Docker explains a failed task");
        docker
            .remove_jobs(&self.labels, JobRuns::Except("operation-2"))
            .await
            .unwrap();
        assert_ne!(
            docker.job_status(&second).await.unwrap(),
            JobStatus::Missing
        );
        docker
            .remove_jobs(&self.labels, JobRuns::All)
            .await
            .unwrap();
        assert_eq!(
            docker.job_status(&second).await.unwrap(),
            JobStatus::Missing
        );
    }

    /// Job removal waits for a run's container to stop even after an earlier
    /// attempt deleted its service but failed waiting. `sh` as PID 1 ignores
    /// SIGTERM, so the container outlives its service by the stop grace period.
    async fn remove_jobs_waits_for_stopping_containers(&self) {
        let docker = &self.engine.docker;
        let third = self.job("operation-3", "sleep 300");
        docker.start_job(&third).await.unwrap();
        let running = || async {
            self.docker_api(
                "GET",
                "/containers/json?filters=%7B%22label%22%3A%5B%22io.piqueld.job-operation%3Doperation-3%22%5D%7D",
            )
            .await
            .as_array()
            .unwrap()
            .len()
        };
        tokio::time::timeout(Duration::from_mins(1), async {
            while running().await == 0 {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await
        .expect("job container starts");
        self.docker_api("DELETE", &format!("/services/{}", third.container.name))
            .await;
        docker
            .remove_jobs(&self.labels, JobRuns::All)
            .await
            .unwrap();
        assert_eq!(running().await, 0);
    }

    /// Sends one request to Docker's HTTP API and returns its JSON body.
    async fn docker_api(&self, method: &str, path: &str) -> serde_json::Value {
        use std::io::{Read, Write};
        let socket = self.engine.socket.clone();
        let request = format!("{method} {path} HTTP/1.0\r\nHost: docker\r\n\r\n");
        let response = tokio::task::spawn_blocking(move || {
            let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
            stream.write_all(request.as_bytes()).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        })
        .await
        .unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        assert!(head.contains(" 200 "), "{head}");
        if body.is_empty() {
            return serde_json::Value::Null;
        }
        serde_json::from_str(body).unwrap()
    }

    /// Polls a job run until it stops.
    async fn wait_job(docker: &BollardDocker, job: &piqueld_core::DesiredJobRun) -> JobStatus {
        tokio::time::timeout(Duration::from_mins(1), async {
            loop {
                match docker.job_status(job).await.unwrap() {
                    JobStatus::Running => tokio::time::sleep(Duration::from_millis(250)).await,
                    status => return status,
                }
            }
        })
        .await
        .expect("job finishes")
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
        // Swarm removes service objects before their tasks release the network.
        loop {
            match self
                .engine
                .docker
                .remove_network(self.network.name.as_str(), &self.labels)
                .await
            {
                Ok(()) => break,
                Err(DockerError::RequestSource { source, .. })
                    if matches!(source.downcast_ref::<bollard::errors::Error>(),
                        Some(bollard::errors::Error::DockerResponseServerError { status_code: 400, message })
                            if message.contains("is in use by task")) =>
                {
                    assert!(
                        tokio::time::Instant::now() < removal_deadline,
                        "network task cleanup timed out: {source}"
                    );
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(error) => panic!("network removal failed: {error:?}"),
            }
        }
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
        let secret_name = &self.service.secrets[0].secret_name;
        self.engine
            .docker
            .remove_secrets(std::slice::from_ref(secret_name), &self.labels)
            .await
            .unwrap();
        assert!(matches!(
            raw.inspect_secret(secret_name).await,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404,
                ..
            })
        ));
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
async fn swarm_init_create_replica_drift_restart_jobs_delete_and_volume_retention() {
    let mut scenario = SwarmScenario::new().await;
    scenario.assert_logs().await;
    scenario.assert_exec().await;
    let http_service = scenario.add_http_service().await;
    scenario.assert_healthchecks(&http_service).await;
    scenario.scale_and_reconnect().await;
    scenario.assert_idempotence_and_repair_drift().await;
    scenario.run_jobs().await;
    scenario.remove_jobs_waits_for_stopping_containers().await;
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
    let environment = EnvironmentId::default_for(app.id());
    let owner = InstanceId::parse("git-build-test").unwrap();
    let runtime = piqueld::application::ApplicationRuntime::new(
        std::sync::Arc::new(docker.clone()),
        owner.clone(),
        std::sync::Arc::new(tokio::sync::Notify::new()),
        Duration::from_mins(2),
        std::sync::Arc::default(),
    );
    let target = runtime
        .prepare(&environment, &app, &piqueld_core::ResolutionSet::default())
        .await
        .unwrap();
    docker.ensure_network(&target.networks[0]).await.unwrap();
    let observed = docker.observe(&environment).await.unwrap();
    assert!(observed.networks[0].runtime_configuration_matches);
    docker.ensure_network(&target.networks[0]).await.unwrap();
    engine.ensure_service_eventually(&target.services[0]).await;
    tokio::time::timeout(Duration::from_mins(1), async {
        loop {
            let observed = docker.observe(&environment).await.unwrap();
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

    // The build carries its installation's labels: another installation
    // can't remove it, and its own can once no container uses it.
    let id = piqueld_core::Sha256Digest::parse(target.services[0].image.as_str()).unwrap();
    let built = || async {
        docker
            .images()
            .await
            .unwrap()
            .into_iter()
            .find(|image| image.id == id)
    };
    let other = InstanceId::parse("another-installation").unwrap();
    let image = built().await.expect("the build is listed by its ID");
    assert!(image.built_by(&owner) && !image.built_by(&other));
    assert!(matches!(
        docker.remove_image(&other, &id).await,
        Err(DockerError::OwnershipConflict)
    ));
    tokio::time::timeout(Duration::from_mins(1), async {
        while built().await.is_some_and(|image| image.used) {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .expect("the removed service's containers go");
    docker.remove_image(&owner, &id).await.unwrap();
    assert!(built().await.is_none());
}
