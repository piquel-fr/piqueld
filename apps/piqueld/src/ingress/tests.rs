mod acme;
mod dns01;
mod traffic;
mod tunnel;
use super::*;
use crate::{
    api::{Mutation, MutationResponse},
    docker::{BollardDocker, DockerApi},
    reconcile::Controller,
};
use hyper::Method;
use piqueld_core::{
    EnvironmentId, NormalizedApplication, OperationState, manifest::ApplicationTemplate,
    observability::EventScope,
};
use serde_json::json;

fn application(name: &str, host: &str, body: &str) -> NormalizedApplication {
    piqueld_core::parse_toml(&format!("api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='{name}'\n[[spec.services]]\nname='web'\ncommand=['caddy']\narguments=['respond','--listen',':8080','--body','{body}']\n[spec.services.source]\ntype='image'\nimage='{CADDY_IMAGE}'\n[[spec.routes]]\nhostname='{host}'\nvisibility='public'\nservice='web'\nport=8080")).unwrap().normalize(piqueld_core::ApplicationId::parse("input-app").unwrap())
}

async fn request_deployment(store: &Store, app: NormalizedApplication) -> (EnvironmentId, String) {
    let (MutationResponse::Saved(saved), _) = store
        .accept(
            crate::api::Actor::Daemon,
            Mutation::Save {
                application: Box::new(ApplicationTemplate::from(&app)),
                expected_application_id: None,
                deploy: true,
            },
            None,
            true,
            None,
        )
        .await
        .unwrap()
    else {
        panic!("saved")
    };
    let id = EnvironmentId::parse(saved.application_id).unwrap();
    let operation = saved.operation_id.unwrap();
    (id, operation)
}

async fn deploy(
    store: &Store,
    controller: &Controller<BollardDocker>,
    app: NormalizedApplication,
) -> EnvironmentId {
    let (id, operation) = request_deployment(store, app).await;
    tokio::time::timeout(Duration::from_mins(3), async {
        loop {
            controller.scan(&CancellationToken::new()).await.unwrap();
            let status = store.operation(&operation).await.unwrap();
            if status.state == OperationState::Succeeded {
                break;
            }
            eprintln!("deploy: {:?} {:?}", status.state, status.error_message);
            if status.state == OperationState::Failed {
                assert_ne!(
                    status.error_code.as_deref(),
                    Some("ingress_unavailable"),
                    "healthy ingress deployment must not need a retry: {:?}",
                    status.error_message
                );
                store.retry_operation(&status).await.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .expect("deployment converges");
    id
}

struct Scenario {
    directory: tempfile::TempDir,
    socket: PathBuf,
    store: Arc<Store>,
    docker: Arc<BollardDocker>,
    gateway: Arc<Ingress>,
    controller: Controller<BollardDocker>,
    first: EnvironmentId,
    client: reqwest::Client,
    tls_port: u16,
    plain_port: u16,
}

impl Scenario {
    async fn new() -> Self {
        assert_eq!(std::env::var("PIQUELD_DOCKER_ISOLATED").as_deref(), Ok("1"));
        let socket = PathBuf::from(std::env::var("PIQUELD_DOCKER_SOCKET").unwrap());
        assert_ne!(
            std::fs::canonicalize(&socket).unwrap(),
            PathBuf::from("/run/docker.sock")
        );
        let root = PathBuf::from(std::env::var("PIQUELD_DOCKER_DATA_DIR").unwrap());
        let directory = tempfile::tempdir_in(root).unwrap();
        let store = Arc::new(Store::open(directory.path().join("db")).await.unwrap());
        let docker = Arc::new(BollardDocker::connect(&socket).unwrap());
        docker.ensure_swarm(true).await.unwrap();
        let mut gateway =
            Ingress::new(true, &socket, directory.path(), Arc::clone(&store)).unwrap();
        // Test-only private CA: no public domain or ACME rate limits are needed.
        gateway.issuer = Some(json!({"module":"internal"}));
        gateway.extra_hosts = vec![
            "http-acme.example.test:127.0.0.1".into(),
            "alpn-acme.example.test:127.0.0.1".into(),
        ];
        gateway.private_port = Some(
            std::env::var("PIQUELD_INGRESS_PRIVATE_PORT")
                .unwrap()
                .parse()
                .unwrap(),
        );
        let gateway = Arc::new(gateway);
        let controller = Controller::new(Arc::clone(&docker), Arc::clone(&store))
            .with_ingress(Arc::clone(&gateway));
        let first = deploy(
            &store,
            &controller,
            application("one", "one.example.test", "first backend"),
        )
        .await;
        let tls_port = std::env::var("PIQUELD_INGRESS_HTTPS_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let plain_port = std::env::var("PIQUELD_INGRESS_HTTP_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let root_cert = tokio::fs::read(
            directory
                .path()
                .join("ingress/data/caddy/pki/authorities/local/root.crt"),
        )
        .await
        .unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            // Caddy closes idle connections on reload. Probe listener availability
            // across cutover without racing a reused client connection.
            .http1_only()
            .pool_max_idle_per_host(0)
            .redirect(reqwest::redirect::Policy::none())
            .add_root_certificate(reqwest::Certificate::from_pem(&root_cert).unwrap())
            .resolve("one.example.test", "127.0.0.1:443".parse().unwrap())
            .resolve("two.example.test", "127.0.0.1:443".parse().unwrap())
            .resolve("www.example.test", "127.0.0.1:443".parse().unwrap())
            .resolve("three.example.test", "127.0.0.1:443".parse().unwrap())
            .build()
            .unwrap();
        Self {
            directory,
            socket,
            store,
            docker,
            gateway,
            controller,
            first,
            client,
            tls_port,
            plain_port,
        }
    }

    fn url(&self, host: &str) -> String {
        format!("https://{host}:{}/", self.tls_port)
    }

    async fn body(&self, host: &str) -> String {
        self.client
            .get(self.url(host))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    }

    /// Caddy's graceful reload may close a connection it accepted just before
    /// the handoff without reading the request. Clients retry such idempotent
    /// requests once; routing itself must still answer on that retry.
    async fn body_across_reload(&self, host: &str) -> String {
        match self.client.get(self.url(host)).send().await {
            Ok(response) => response.text().await.unwrap(),
            Err(error) if error.is_request() && !error.is_timeout() => self.body(host).await,
            Err(error) => panic!("request failed across reload: {error:?}"),
        }
    }

    async fn assert_public_routing(&self) {
        // Deployment journaled the gateway start; a converged gateway records nothing.
        let history = self.store.events(None, None, 100).await.unwrap().items;
        assert!(history.iter().any(|event| event.kind == "action_succeeded"
            && event.phase.as_deref() == Some("ingress_start_gateway")));
        let retained = history.len();
        self.gateway.synchronize().await.unwrap();
        assert_eq!(
            self.store
                .events(None, None, 100)
                .await
                .unwrap()
                .items
                .len(),
            retained
        );
        let container = self.gateway.container().await.unwrap().unwrap();
        assert_eq!(
            container["HostConfig"]["PortBindings"]["443/tcp"][0]["HostPort"],
            "443"
        );
        assert_eq!(
            container["NetworkSettings"]["Networks"][&self.gateway.name]["GwPriority"],
            1
        );
        assert_eq!(self.body("one.example.test").await, "first backend");
        let redirect = self
            .client
            .get(format!("http://127.0.0.1:{}/path?q=1", self.plain_port))
            .header("Host", "one.example.test")
            .send()
            .await
            .unwrap();
        assert_eq!(redirect.status(), 308);
        assert_eq!(
            redirect.headers()["location"],
            "https://one.example.test/path?q=1"
        );
        assert_eq!(
            self.client
                .get(format!("http://127.0.0.1:{}/", self.plain_port))
                .header("Host", "unknown.example.test")
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
    }

    /// A service-less application's redirect is answered by Caddy itself, so
    /// the application has no ingress network.
    async fn redirect_without_backend(&self) {
        let app = piqueld_core::parse_toml("api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='www'\n[[spec.routes]]\nhostname='www.example.test'\nvisibility='public'\nredirect={to='https://one.example.test/base/'}").unwrap().normalize(piqueld_core::ApplicationId::parse("input-app").unwrap());
        let id = deploy(&self.store, &self.controller, app).await;
        let response = self
            .client
            .get(format!("{}path?q=1", self.url("www.example.test")))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 308);
        assert_eq!(
            response.headers()["location"],
            "https://one.example.test/base/path?q=1"
        );
        assert_eq!(self.docker.observe(&id).await.unwrap().networks, []);
    }

    async fn add_application_without_interrupting_traffic(&self) {
        let original = self.gateway.container().await.unwrap().unwrap()["Id"].clone();
        let completed = std::sync::atomic::AtomicBool::new(false);
        let (second, requests) = tokio::join!(
            async {
                let id = deploy(
                    &self.store,
                    &self.controller,
                    application("two", "two.example.test", "second backend"),
                )
                .await;
                completed.store(true, std::sync::atomic::Ordering::SeqCst);
                id
            },
            async {
                let mut requests = 0;
                while !completed.load(std::sync::atomic::Ordering::SeqCst) {
                    assert_eq!(
                        self.body_across_reload("one.example.test").await,
                        "first backend"
                    );
                    requests += 1;
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                requests
            }
        );
        assert!(requests > 0);
        assert_eq!(
            self.gateway.container().await.unwrap().unwrap()["Id"],
            original,
            "network attachment must not replace Caddy"
        );
        assert_eq!(self.body("two.example.test").await, "second backend");
        let one = self.docker.observe(&self.first).await.unwrap();
        let two = self.docker.observe(&second).await.unwrap();
        assert!(
            one.services[0]
                .networks
                .iter()
                .all(|network| !two.services[0].networks.contains(network)),
            "applications do not share backend networks"
        );
        assert_eq!(one.services[0].networks.len(), 2);
    }

    async fn slow_gateway_does_not_block_private_deployment(&self) {
        let guard = self.gateway.update.lock().await;
        let (_, routed) = request_deployment(
            &self.store,
            application("one", "one.example.test", "first backend"),
        )
        .await;
        let cancellation = CancellationToken::new();
        tokio::join!(
            async {
                self.controller
                    .run(
                        Arc::new(tokio::sync::Notify::new()),
                        Duration::from_millis(100),
                        30,
                        30,
                        cancellation.clone(),
                    )
                    .await
                    .unwrap();
            },
            async {
                tokio::time::timeout(Duration::from_mins(1), async {
                    while self
                        .store
                        .operation(&routed)
                        .await
                        .unwrap()
                        .phase
                        .as_deref()
                        != Some("routing")
                    {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    let mut private =
                        application("private", "private.example.test", "private backend")
                            .to_manifest();
                    private.spec.routes.clear();
                    let (_, operation) = request_deployment(
                        &self.store,
                        private.validate().unwrap().normalize(
                            piqueld_core::ApplicationId::parse("private-input").unwrap(),
                        ),
                    )
                    .await;
                    while self.store.operation(&operation).await.unwrap().state
                        != OperationState::Succeeded
                    {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    assert_eq!(
                        self.store.operation(&routed).await.unwrap().state,
                        OperationState::Running
                    );
                })
                .await
                .expect("private deployment proceeds while gateway writer is blocked");
                drop(guard);
                tokio::time::timeout(Duration::from_mins(1), async {
                    while self.store.operation(&routed).await.unwrap().state
                        != OperationState::Succeeded
                    {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                })
                .await
                .unwrap();
                cancellation.cancel();
            }
        );
    }

    async fn reject_invalid_replacement_configuration(
        &self,
        table: &crate::store::ingress::RoutingTable,
        networks: &std::collections::BTreeSet<String>,
    ) {
        let mut invalid = Ingress::new(
            true,
            &self.socket,
            self.directory.path(),
            Arc::clone(&self.store),
        )
        .unwrap();
        invalid.issuer = Some(json!({"module":"not-a-real-issuer"}));
        let original = self.gateway.container().await.unwrap().unwrap()["Id"].clone();
        let error = invalid
            .replace_gateway(table, networks, &invalid.container_spec())
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("configuration validation failed"),
            "{error:#}"
        );
        assert_eq!(
            self.gateway.container().await.unwrap().unwrap()["Id"],
            original
        );
        assert_eq!(self.body("one.example.test").await, "first backend");
    }

    async fn replacement_preserves_unverified_attachment(&self) {
        let original = self.gateway.container().await.unwrap().unwrap()["Id"].clone();
        let table = self.store.routing_table().await.unwrap();
        // A retained route still works on the old attachment, but a replacement
        // cannot inherit it unless its network passed validation.
        self.gateway
            .replace_gateway(
                &table,
                &std::collections::BTreeSet::new(),
                &self.gateway.container_spec(),
            )
            .await
            .unwrap();
        assert_eq!(
            self.gateway.container().await.unwrap().unwrap()["Id"],
            original
        );
        assert_eq!(self.body("one.example.test").await, "first backend");
    }

    async fn replacement_failure_and_recovery(&self) {
        let original = self.gateway.container().await.unwrap().unwrap()["Id"].clone();
        let table = self.store.routing_table().await.unwrap();
        let (_, networks, _) = self.gateway.prepare_routes(&table).await.unwrap();
        let mut spec = self.gateway.container_spec();
        spec["Image"] = "piqueld-missing-test-image:never-pull".into();
        assert!(
            self.gateway
                .replace_gateway(&table, &networks, &spec)
                .await
                .is_err()
        );
        assert_eq!(
            self.gateway.container().await.unwrap().unwrap()["Id"],
            original
        );
        assert_eq!(self.body("one.example.test").await, "first backend");

        self.reject_invalid_replacement_configuration(&table, &networks)
            .await;

        let mut spec = self.gateway.container_spec();
        spec["Cmd"] = json!(["/does-not-exist"]);
        let error = self
            .gateway
            .replace_gateway(&table, &networks, &spec)
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("previous gateway restored"),
            "{error:#}"
        );
        assert_eq!(
            self.gateway.container().await.unwrap().unwrap()["Id"],
            original
        );
        assert_eq!(self.body("one.example.test").await, "first backend");

        // Simulate process termination after the old gateway was stopped. Recovery
        // uses Docker names and the persisted snapshot, not in-memory state.
        let configuration = self.gateway.caddy.get("/config/").await.unwrap();
        tokio::fs::write(
            self.directory
                .path()
                .join("ingress/config/caddy/rollback.json"),
            serde_json::to_vec(&configuration).unwrap(),
        )
        .await
        .unwrap();
        let previous = format!("{}-previous", self.gateway.name);
        self.gateway
            .docker
            .external(
                Method::POST,
                &format!("/containers/{}/stop?t=1", self.gateway.name),
                None,
            )
            .await
            .unwrap();
        self.gateway
            .docker
            .external(
                Method::POST,
                &format!("/containers/{}/rename?name={previous}", self.gateway.name),
                None,
            )
            .await
            .unwrap();
        self.gateway.recover_gateway().await.unwrap();
        assert_eq!(
            self.gateway.container().await.unwrap().unwrap()["Id"],
            original
        );
        assert_eq!(self.body("one.example.test").await, "first backend");

        self.gateway
            .replace_gateway(&table, &networks, &self.gateway.container_spec())
            .await
            .unwrap();
        assert_ne!(
            self.gateway.container().await.unwrap().unwrap()["Id"],
            original
        );
        assert_eq!(self.body("one.example.test").await, "first backend");
        assert!(
            self.gateway
                .docker
                .inspect(&format!("/containers/{previous}/json"))
                .await
                .unwrap()
                .is_none()
        );
    }

    /// A committed replacement is never reverted because cleanup failed.
    async fn committed_replacement_survives_stale_cleanup(&self) {
        let previous = format!("{}-previous", self.gateway.name);
        let replaced = self.gateway.container().await.unwrap().unwrap()["Id"].clone();
        self.gateway
            .docker
            .external(
                Method::POST,
                &format!("/containers/create?name={previous}"),
                Some(&self.gateway.container_spec()),
            )
            .await
            .unwrap();
        self.gateway.recover_gateway().await.unwrap();
        assert_eq!(
            self.gateway.container().await.unwrap().unwrap()["Id"],
            replaced
        );
        assert!(
            self.gateway
                .docker
                .inspect(&format!("/containers/{previous}/json"))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(self.body("one.example.test").await, "first backend");
    }

    async fn unavailable_application(&self) -> (EnvironmentId, NormalizedApplication) {
        let app = application("unavailable", "unavailable.example.test", "unavailable");
        let (MutationResponse::Saved(saved), _) = self
            .store
            .accept(
                crate::api::Actor::Daemon,
                Mutation::Save {
                    application: Box::new(ApplicationTemplate::from(&app)),
                    expected_application_id: None,
                    deploy: false,
                },
                None,
                true,
                None,
            )
            .await
            .unwrap()
        else {
            panic!("saved")
        };
        let unavailable = EnvironmentId::parse(saved.application_id).unwrap();
        (unavailable, app)
    }

    async fn unavailable_network_does_not_block_withdrawals(&self) {
        let (unavailable, app) = self.unavailable_application().await;
        self.store
            .stage_routes(&unavailable, &app.spec().routes, true, None)
            .await
            .unwrap();
        // Model an accepted route whose network subsequently disappeared.
        self.store
            .acknowledge_routes(&self.store.routing_table().await.unwrap())
            .await
            .unwrap();
        let mut pending = app.spec().routes.clone();
        pending[0].target = serde_json::from_value(json!({"service":"web","port":8081})).unwrap();
        self.store
            .stage_routes(&unavailable, &pending, true, None)
            .await
            .unwrap();
        let original = self.gateway.container().await.unwrap().unwrap()["Id"].clone();
        let mut upgraded = Ingress::new(
            true,
            &self.socket,
            self.directory.path(),
            Arc::clone(&self.store),
        )
        .unwrap();
        upgraded.issuer = self.gateway.issuer.clone();
        upgraded.extra_hosts = self.gateway.extra_hosts.clone();
        upgraded.private_port = self.gateway.private_port;
        upgraded
            .extra_hosts
            .push("upgrade.example.test:127.0.0.1".into());
        upgraded.synchronize_for(Some(&self.first)).await.unwrap();
        assert!(!upgraded.status().await.healthy);
        assert_eq!(
            self.store.applied_routes(&unavailable).await.unwrap(),
            app.spec().routes
        );
        assert_eq!(self.body("one.example.test").await, "first backend");
        assert_eq!(upgraded.container().await.unwrap().unwrap()["Id"], original);
        let network = piqueld_core::DockerNetworkName::for_ingress(&unavailable).to_string();
        self.gateway
            .docker
            .external(
                Method::POST,
                "/networks/create",
                Some(&json!({"Name":network,"Driver":"bridge"})),
            )
            .await
            .unwrap();
        upgraded.synchronize_for(Some(&self.first)).await.unwrap();
        assert!(
            self.gateway.container().await.unwrap().unwrap()["NetworkSettings"]["Networks"]
                .get(&network)
                .is_none(),
            "a conflicting network must never be attached"
        );
        let routes = self.store.applied_routes(&self.first).await.unwrap();
        self.store
            .stage_routes(&self.first, &[], true, None)
            .await
            .unwrap();
        upgraded.synchronize_for(Some(&self.first)).await.unwrap();
        assert_eq!(
            self.store.applied_routes(&self.first).await.unwrap(),
            [] as [piqueld_core::manifest::ValidatedRoute; 0]
        );
        assert_eq!(
            self.client
                .get(format!("http://127.0.0.1:{}/", self.plain_port))
                .header("Host", "one.example.test")
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        // Healthy applications can also restore/add routes while this app is broken.
        self.store
            .stage_routes(&self.first, &routes, true, None)
            .await
            .unwrap();
        upgraded.synchronize_for(Some(&self.first)).await.unwrap();
        assert_eq!(self.body("one.example.test").await, "first backend");
        assert_eq!(upgraded.container().await.unwrap().unwrap()["Id"], original);
        self.store
            .stage_routes(&unavailable, &[], true, None)
            .await
            .unwrap();
        upgraded.synchronize().await.unwrap();
        assert!(upgraded.status().await.healthy);
        assert_ne!(upgraded.container().await.unwrap().unwrap()["Id"], original);
        assert_eq!(self.body("one.example.test").await, "first backend");
        self.gateway
            .docker
            .external(Method::DELETE, &format!("/networks/{network}"), None)
            .await
            .unwrap();
        // Restore the controller's specification before testing ordinary reloads.
        self.gateway.synchronize().await.unwrap();
    }

    async fn restart_without_daemon(&self) {
        self.gateway
            .docker
            .external(
                Method::POST,
                &format!("/containers/{}/restart?t=1", self.gateway.name),
                None,
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(response) = self.client.get(self.url("one.example.test")).send().await
                    && response.status().is_success()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(self.body("one.example.test").await, "first backend");
    }

    async fn release_unhealthy_backend(&self) {
        // Wait for a running but unhealthy replacement and prove the old
        // backend keeps serving before releasing the health check.
        let next_name = piqueld_core::DockerServiceName::for_service(
            &self.first,
            &piqueld_core::ServiceName::parse("next").unwrap(),
        );
        let container = tokio::time::timeout(Duration::from_mins(1), async {
            loop {
                let containers = self.gateway.docker.get("/containers/json").await.unwrap();
                if let Some(container) = containers.as_array().unwrap().iter().find(|container| {
                    container["Labels"]["com.docker.swarm.service.name"] == next_name.as_str()
                }) {
                    break container["Id"].as_str().unwrap().to_owned();
                }
                assert_eq!(
                    self.body_across_reload("one.example.test").await,
                    "first backend"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        for _ in 0..5 {
            assert_eq!(
                self.body_across_reload("one.example.test").await,
                "first backend"
            );
            assert_eq!(
                self.store.applied_routes(&self.first).await.unwrap()[0]
                    .target
                    .service()
                    .unwrap()
                    .as_str(),
                "web"
            );
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let health = self
            .gateway
            .docker
            .inspect(&format!("/containers/{container}/json"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(health["State"]["Running"], true);
        assert!(
            health["State"]["Health"]["Log"]
                .as_array()
                .unwrap()
                .iter()
                .any(|probe| probe["ExitCode"] == 1),
            "cutover must remain blocked after a real failed health probe"
        );
        let exec = self
            .gateway
            .docker
            .external(
                Method::POST,
                &format!("/containers/{container}/exec"),
                Some(&json!({"Cmd":["touch","/tmp/ready"]})),
            )
            .await
            .unwrap();
        self.gateway
            .docker
            .external(
                Method::POST,
                &format!("/exec/{}/start", exec["Id"].as_str().unwrap()),
                Some(&json!({"Detach":true})),
            )
            .await
            .unwrap();
    }

    async fn repoint_after_backend_convergence(&self) {
        let mut changed = application("one", "one.example.test", "first backend").to_manifest();
        let mut next = changed.spec.services[0].clone();
        next.name = "next".into();
        next.arguments = vec![
            "respond",
            "--listen",
            ":8080",
            "--body",
            "replacement backend",
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        next.healthcheck = Some(piqueld_core::manifest::HealthCheck::Command {
            // Keep the replacement unhealthy until the test explicitly releases it.
            command: vec!["test".into(), "-f".into(), "/tmp/ready".into()],
            interval_seconds: 3.into(),
            timeout_seconds: 1.into(),
        });
        changed.spec.services.push(next);
        changed.spec.routes[0].service = Some("next".into());
        let completed = std::sync::atomic::AtomicBool::new(false);
        tokio::join!(
            async {
                Box::pin(deploy(
                    &self.store,
                    &self.controller,
                    changed.validate().unwrap().normalize(
                        piqueld_core::ApplicationId::parse(self.first.as_str()).unwrap(),
                    ),
                ))
                .await;
                completed.store(true, std::sync::atomic::Ordering::SeqCst);
            },
            async {
                self.release_unhealthy_backend().await;
                while !completed.load(std::sync::atomic::Ordering::SeqCst) {
                    let body = self.body_across_reload("one.example.test").await;
                    assert!(
                        matches!(body.as_str(), "first backend" | "replacement backend"),
                        "{body}"
                    );
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
        );
        let observed = self.docker.observe(&self.first).await.unwrap();
        let web = piqueld_core::DockerServiceName::for_service(
            &self.first,
            &piqueld_core::ServiceName::parse("web").unwrap(),
        );
        assert_eq!(
            observed
                .services
                .iter()
                .find(|service| service.name == web.as_str())
                .unwrap()
                .networks
                .len(),
            1,
            "previous backend loses its ingress attachment only after cutover"
        );
        assert_eq!(
            self.store.applied_routes(&self.first).await.unwrap()[0]
                .target
                .service()
                .unwrap()
                .as_str(),
            "next"
        );
        assert_eq!(self.body("one.example.test").await, "replacement backend");
    }

    /// Backends see Caddy's forwarded client address from a peer inside the
    /// injected ingress range; a client-supplied forwarding header is discarded.
    async fn forwarded_client_addresses(&self) {
        let id = deploy(
            &self.store,
            &self.controller,
            application(
                "three",
                "three.example.test",
                "{http.request.remote.host} {http.request.header.X-Forwarded-For} {env.PIQUELD_INGRESS_PROXIES}",
            ),
        )
        .await;
        let body = self
            .client
            .get(self.url("three.example.test"))
            .header("X-Forwarded-For", "203.0.113.9")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let [peer, forwarded, proxies] = body.split(' ').collect::<Vec<_>>()[..] else {
            panic!("unexpected backend response: {body}");
        };
        let peer: std::net::Ipv4Addr = peer.parse().unwrap();
        forwarded.parse::<std::net::IpAddr>().unwrap();
        assert_ne!(
            forwarded, "203.0.113.9",
            "client-supplied header reached the backend"
        );
        let (network, prefix) = proxies.split_once('/').unwrap();
        let mask = u32::MAX << (32 - prefix.parse::<u32>().unwrap());
        assert_eq!(
            u32::from(peer) & mask,
            u32::from(network.parse::<std::net::Ipv4Addr>().unwrap()),
            "{peer} is outside {proxies}"
        );
        let observed = self.docker.observe(&id).await.unwrap();
        assert!(observed.services[0].runtime_configuration_matches);
        assert!(
            !observed.services[0]
                .environment
                .contains_key(piqueld_core::resource::INGRESS_PROXIES_ENV)
        );
    }

    async fn disable_and_restore_deployed_routes(&self) {
        let disabled = Arc::new(
            Ingress::new(
                false,
                &self.socket,
                self.directory.path(),
                Arc::clone(&self.store),
            )
            .unwrap(),
        );
        disabled.synchronize().await.unwrap();
        assert!(disabled.container().await.unwrap().is_none());
        assert!(
            self.client
                .get(self.url("one.example.test"))
                .send()
                .await
                .is_err()
        );
        assert!(
            self.directory
                .path()
                .join("ingress/data/caddy/pki/authorities/local/root.crt")
                .exists()
        );
        let controller = Controller::new(Arc::clone(&self.docker), Arc::clone(&self.store))
            .with_ingress(Arc::clone(&disabled));
        let mut changed =
            application("one", "one.example.test", "updated while disabled").to_manifest();
        changed.spec.routes.clear();
        deploy(
            &self.store,
            &controller,
            changed
                .validate()
                .unwrap()
                .normalize(piqueld_core::ApplicationId::parse(self.first.as_str()).unwrap()),
        )
        .await;
        assert_eq!(
            self.store.routing_table().await.unwrap()[&self.first],
            [] as [piqueld_core::manifest::ValidatedRoute; 0]
        );
        assert_eq!(
            self.docker.observe(&self.first).await.unwrap().services[0]
                .networks
                .len(),
            1
        );
        tokio::time::timeout(Duration::from_secs(90), async {
            loop {
                self.controller
                    .scan(&CancellationToken::new())
                    .await
                    .unwrap();
                // Re-enable may recreate Swarm endpoints; route application and
                // public backend reachability are deliberately separate states.
                if self.gateway.synchronize().await.is_ok()
                    && let Ok(response) = self.client.get(self.url("two.example.test")).send().await
                    && response.status().is_success()
                    && response
                        .text()
                        .await
                        .is_ok_and(|body| body == "second backend")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            self.client
                .get(format!("http://127.0.0.1:{}/", self.plain_port))
                .header("Host", "one.example.test")
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        assert_eq!(self.body("two.example.test").await, "second backend");
        disabled.synchronize().await.unwrap();
    }
}

/// Response body that must never reach history, diagnostics or health messages.
const PRIVATE_BODY: &str = "private-engine-detail";

/// An enabled ingress whose Docker Engine fails every request with `PRIVATE_BODY`.
struct FailingEngine {
    directory: tempfile::TempDir,
    store: Arc<Store>,
    ingress: Ingress,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl FailingEngine {
    async fn start() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("docker.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let engine = axum::Router::new()
            .fallback(|| async { (hyper::StatusCode::INTERNAL_SERVER_ERROR, PRIVATE_BODY) });
        let server = tokio::spawn(async move { axum::serve(listener, engine).await });
        let store = Arc::new(Store::open(directory.path().join("db")).await.unwrap());
        let ingress = Ingress::new(true, &socket, directory.path(), Arc::clone(&store)).unwrap();
        Self {
            directory,
            store,
            ingress,
            server,
        }
    }

    async fn history(&self) -> Vec<piqueld_core::Event> {
        let events = self.store.events(None, None, 100).await.unwrap().items;
        assert!(
            !serde_json::to_string(&events)
                .unwrap()
                .contains(PRIVATE_BODY)
        );
        events
    }
}

impl Drop for FailingEngine {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[tokio::test]
async fn journaled_requests_commit_intent_and_record_sanitized_failures() {
    let engine = FailingEngine::start().await;
    let journal = engine
        .ingress
        .journal("ingress_create_network", "edge")
        .await
        .unwrap();
    let result = engine
        .ingress
        .docker
        .send(&journal, Method::POST, "/networks/create", None)
        .await;
    journal.finish(result).await.unwrap_err();

    let events = engine.history().await;
    let kinds: Vec<_> = events.iter().map(|event| event.kind.as_str()).collect();
    assert_eq!(
        kinds,
        ["action_started", "action_requested", "action_failed"]
    );
    let failed = &events[2];
    assert_eq!(failed.scope, EventScope::Daemon);
    assert_eq!(failed.error_code.as_deref(), Some("ingress_unavailable"));
    assert!(
        failed
            .diagnostic
            .as_ref()
            .unwrap()
            .causes
            .contains(&"Ingress API returned HTTP status 500".to_owned())
    );
}

#[tokio::test]
async fn failed_gateway_sync_reports_health_without_response_details() {
    let engine = FailingEngine::start().await;
    engine.ingress.synchronize().await.unwrap_err();

    // The failure happened while reading Docker, so no action was journaled.
    let events = engine.history().await;
    let [health] = events.as_slice() else {
        panic!("expected one health transition: {events:#?}")
    };
    assert_eq!(health.kind, "dependency_health_changed");
    assert_eq!(health.scope, EventScope::Daemon);
    assert_eq!(health.error_code.as_deref(), Some("ingress_unavailable"));
    let status = engine.ingress.status().await;
    assert!(!status.healthy);
    assert_eq!(
        status.message,
        "Ingress unavailable: could not prepare the Caddy gateway (requires free ports \
         80/443 and Docker 28+). See daemon logs for details."
    );
}

#[tokio::test]
#[ignore = "requires the isolated Docker harness with loopback test ports"]
async fn ingress_caddy_routes_tls_network_changes_and_disable() {
    tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .unwrap();
    let scenario = Box::pin(Scenario::new()).await;
    scenario.assert_public_routing().await;
    scenario.redirect_without_backend().await;
    scenario
        .slow_gateway_does_not_block_private_deployment()
        .await;
    scenario.persistent_traffic_survives_reload().await;
    scenario.replacement_preserves_unverified_attachment().await;
    scenario.replacement_failure_and_recovery().await;
    scenario
        .committed_replacement_survives_stale_cleanup()
        .await;
    scenario
        .unavailable_network_does_not_block_withdrawals()
        .await;
    scenario
        .add_application_without_interrupting_traffic()
        .await;
    scenario.restart_without_daemon().await;
    Box::pin(scenario.repoint_after_backend_convergence()).await;
    scenario.forwarded_client_addresses().await;
    scenario.disable_and_restore_deployed_routes().await;
}

#[tokio::test]
#[ignore = "requires the isolated Docker harness with loopback test ports"]
async fn ingress_caddy_acme_challenges_and_renewal() {
    use futures_util::FutureExt;
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let scenario = Box::pin(Scenario::new()).await;
    let result = std::panic::AssertUnwindSafe(scenario.acme_challenges_and_renewal())
        .catch_unwind()
        .await;
    if result.is_err() {
        scenario.gateway.relay_logs().await.unwrap();
    }
    scenario.gateway.stop_gateway().await.unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
#[ignore = "requires the isolated Docker harness with loopback test ports"]
async fn ingress_caddy_dns01_certificates() {
    use futures_util::FutureExt;
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let scenario = Box::pin(Scenario::new()).await;
    let result =
        Box::pin(std::panic::AssertUnwindSafe(scenario.dns01_certificates()).catch_unwind()).await;
    scenario.stop_pebble().await;
    scenario.gateway.stop_gateway().await.unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// An ingress with private ingress enabled whose Docker Engine is never called.
async fn private_ingress(directory: &tempfile::TempDir) -> Ingress {
    let store = Arc::new(Store::open(directory.path().join("db")).await.unwrap());
    // Docker clients require the socket file to exist; nothing listens on it.
    let socket = directory.path().join("docker.sock");
    drop(tokio::net::UnixListener::bind(&socket).unwrap());
    Ingress::new(true, &socket, directory.path(), store)
        .unwrap()
        .with_private(&crate::config::PrivateIngressConfig {
            enabled: true,
            ..Default::default()
        })
}

#[tokio::test]
async fn each_listener_serves_only_its_own_routes() {
    let directory = tempfile::tempdir().unwrap();
    let ingress = private_ingress(&directory).await;
    let routes = |visibility: &str, host: &str| {
        let mut manifest = application("one", host, "body").to_manifest();
        manifest.spec.routes[0].visibility = visibility.parse().unwrap();
        manifest.validate().unwrap().spec().routes.clone()
    };
    let table: crate::store::ingress::RoutingTable = [
        (
            EnvironmentId::parse("env-public").unwrap(),
            routes("public", "www.example.com"),
        ),
        (
            EnvironmentId::parse("env-private").unwrap(),
            routes("private", "admin.example.com"),
        ),
    ]
    .into();
    let hosts = |server: &serde_json::Value| {
        server["routes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|route| route["match"][0]["host"][0].as_str().map(str::to_owned))
            .collect::<std::collections::BTreeSet<_>>()
    };
    let edge = ["172.20.0.0/16".to_owned()];
    let configuration = ingress.build_configuration(&table, Some(&edge), None);
    let servers = &configuration["apps"]["http"]["servers"];
    for (https, http, host) in [
        ("public", "public_http", "www.example.com"),
        ("private", "private_http", "admin.example.com"),
    ] {
        assert_eq!(hosts(&servers[https]), [host.to_owned()].into(), "{https}");
        assert_eq!(hosts(&servers[http]), [host.to_owned()].into(), "{http}");
        // No other hostname completes TLS on this listener.
        assert_eq!(
            servers[https]["tls_connection_policies"][0]["match"]["sni"],
            json!([host])
        );
    }
    // Applications reach the private listener over their networks, so only
    // tailnet client addresses, set by edge peers' PROXY headers, complete TLS.
    let tailnet = json!({"ranges":["100.64.0.0/10", "fd7a:115c:a1e0::/48"]});
    assert_eq!(
        servers["private"]["tls_connection_policies"][0]["match"]["remote_ip"],
        tailnet
    );
    assert_eq!(
        servers["private_http"]["routes"][0]["match"],
        json!([{"not":[{"remote_ip":tailnet}]}])
    );
    assert!(
        servers["public"]["tls_connection_policies"][0]["match"]
            .get("remote_ip")
            .is_none()
    );
    assert_eq!(servers["public"]["listen"], json!([":443"]));
    assert_eq!(servers["private"]["listen"], json!([":8443"]));
    let wrapper = json!({"wrapper":"proxy_protocol","allow":edge,"fallback_policy":"ignore"});
    assert_eq!(
        servers["private"]["listener_wrappers"],
        json!([wrapper, {"wrapper":"tls"}])
    );
    assert_eq!(
        servers["private_http"]["listener_wrappers"],
        json!([wrapper])
    );
    // Private certificates come from DNS-01 only.
    assert_eq!(
        servers["private"]["automatic_https"],
        json!({"disable":true})
    );

    // Without private ingress, private routes are served nowhere, never on
    // the public listener.
    let configuration = ingress.build_configuration(&table, None, None);
    let servers = configuration["apps"]["http"]["servers"]
        .as_object()
        .unwrap();
    assert_eq!(
        servers.keys().map(String::as_str).collect::<Vec<_>>(),
        ["public", "public_http"]
    );
    assert_eq!(
        hosts(&servers["public"]),
        ["www.example.com".to_owned()].into()
    );
}

#[tokio::test]
async fn the_apps_node_forwards_to_the_private_listener_without_privileges() {
    let directory = tempfile::tempdir().unwrap();
    let ingress = private_ingress(&directory).await;
    let gateway = &ingress.name;
    assert_eq!(
        ingress.serve_configuration(),
        json!({"TCP":{
            "443":{"TCPForward":format!("{gateway}:8443"),"ProxyProtocol":2},
            "80":{"TCPForward":format!("{gateway}:8081"),"ProxyProtocol":2}
        }})
    );
    let spec = ingress.node_spec(ingress.node.as_ref().unwrap());
    assert_eq!(spec["HostConfig"]["NetworkMode"], json!(gateway));
    assert_eq!(spec["HostConfig"]["CapDrop"], json!(["ALL"]));
    assert_eq!(spec["HostConfig"]["ReadonlyRootfs"], true);
    assert!(spec["HostConfig"].get("PortBindings").is_none());
    let environment = spec["Env"].as_array().unwrap();
    assert!(environment.contains(&json!("TS_USERSPACE=true")));
    assert!(environment.contains(&json!("TS_HOSTNAME=piqueld-apps")));
    // Without an auth key, the node logs in interactively.
    assert!(
        !environment
            .iter()
            .any(|value| value.as_str().unwrap().starts_with("TS_AUTHKEY"))
    );
    // A replaced auth key replaces the container, which reads it again.
    let hash = |key: &str| {
        let config = crate::config::PrivateIngressConfig {
            enabled: true,
            auth_key: Some(key.into()),
            ..Default::default()
        };
        let node = node::Node::new(&config, &ingress.directory).unwrap();
        ingress.node_spec(&node)["Labels"]["io.piqueld.ingress-configuration"].clone()
    };
    assert_ne!(hash("tskey-auth-old"), hash("tskey-auth-new"));
    assert_ne!(
        hash("tskey-auth-old"),
        spec["Labels"]["io.piqueld.ingress-configuration"]
    );
}

#[tokio::test]
async fn disabled_private_ingress_is_unconfirmed_until_the_node_is_removed() {
    let engine = FailingEngine::start().await;
    engine.ingress.synchronize().await.unwrap_err();
    // The gateway failed, but removing the node was still attempted.
    let private = engine.ingress.status().await.private;
    assert!(!private.enabled && !private.healthy);
    assert!(
        private
            .message
            .starts_with("Removing the apps node is not confirmed"),
        "{}",
        private.message
    );
}

#[tokio::test]
async fn disabled_ingress_confirms_the_node_stopped_only_once_removed() {
    let engine = FailingEngine::start().await;
    let directory = engine.directory.path();
    let disabled = Ingress::new(
        false,
        &directory.join("docker.sock"),
        directory,
        Arc::clone(&engine.store),
    )
    .unwrap()
    .with_private(&crate::config::PrivateIngressConfig {
        enabled: true,
        ..Default::default()
    });
    disabled.synchronize().await.unwrap_err();
    let private = disabled.status().await.private;
    assert!(!private.healthy);
    assert!(
        private
            .message
            .starts_with("Stopping the apps node is not confirmed"),
        "{}",
        private.message
    );
}

/// Whether the private listener is configured on an engine whose Swarm
/// allocates application networks from `pool`.
async fn private_listener_with_pool(pool: &'static str) -> bool {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("docker.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let engine = axum::Router::new()
        .route(
            "/info",
            axum::routing::get(move || async move {
                axum::Json(json!({"Swarm":{"Cluster":{"DefaultAddrPool":[pool]}}}))
            }),
        )
        .fallback(|| async { axum::Json(json!({"IPAM":{"Config":[{"Subnet":"172.20.0.0/16"}]}})) });
    let server = tokio::spawn(async move { axum::serve(listener, engine).await });
    let store = Arc::new(Store::open(directory.path().join("db")).await.unwrap());
    let ingress = Ingress::new(true, &socket, directory.path(), store)
        .unwrap()
        .with_private(&crate::config::PrivateIngressConfig {
            enabled: true,
            ..Default::default()
        });
    let configuration = ingress.configuration(&RoutingTable::new()).await.unwrap();
    server.abort();
    configuration["apps"]["http"]["servers"]
        .get("private")
        .is_some()
}

#[tokio::test]
async fn the_private_listener_stays_off_while_application_pools_overlap_the_tailnet() {
    assert!(private_listener_with_pool("10.0.0.0/8").await);
    // Applications on such a pool could pass for tailnet clients.
    assert!(!private_listener_with_pool("100.64.0.0/10").await);
    assert!(!private_listener_with_pool("not a pool").await);
}
