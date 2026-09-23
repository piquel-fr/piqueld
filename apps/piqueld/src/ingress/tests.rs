use super::*;
use crate::{
    api::{Mutation, MutationResponse},
    docker::{BollardDocker, DockerApi},
    reconcile::Controller,
};
use hyper::Method;
use piqueld_core::{ApplicationId, NormalizedApplication, OperationState};
use serde_json::json;

fn application(name: &str, host: &str, body: &str) -> NormalizedApplication {
    piqueld_core::parse_toml(&format!("api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='{name}'\n[[spec.services]]\nname='web'\ncommand=['caddy']\narguments=['respond','--listen',':8080','--body','{body}']\n[spec.services.source]\ntype='image'\nimage='{CADDY_IMAGE}'\n[[spec.routes]]\nhostname='{host}'\nservice='web'\nport=8080")).unwrap().normalize(ApplicationId::parse("input-app").unwrap())
}

async fn deploy(
    store: &Store,
    controller: &Controller<BollardDocker>,
    app: NormalizedApplication,
) -> ApplicationId {
    let (MutationResponse::Saved(saved), _) = store
        .accept(
            Mutation::Save {
                application: app,
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
    let id = ApplicationId::parse(saved.application_id).unwrap();
    let operation = saved.operation_id.unwrap();
    tokio::time::timeout(Duration::from_secs(180), async {
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
    first: ApplicationId,
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

    async fn assert_public_routing(&self) {
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
                    assert_eq!(self.body("one.example.test").await, "first backend");
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

    async fn restart_without_daemon(&self) {
        self.gateway
            .docker
            .json(
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

    async fn repoint_after_backend_convergence(&self) {
        let mut changed = application("one", "one.example.test", "first backend").to_manifest();
        let mut next = changed.spec.services[0].clone();
        next.name = "next".into();
        changed.spec.services.push(next);
        changed.spec.routes[0].service = "next".into();
        let completed = std::sync::atomic::AtomicBool::new(false);
        tokio::join!(
            async {
                deploy(
                    &self.store,
                    &self.controller,
                    changed.validate().unwrap().normalize(self.first.clone()),
                )
                .await;
                completed.store(true, std::sync::atomic::Ordering::SeqCst);
            },
            async {
                while !completed.load(std::sync::atomic::Ordering::SeqCst) {
                    assert_eq!(self.body("one.example.test").await, "first backend");
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
                .service
                .as_str(),
            "next"
        );
        assert_eq!(self.body("one.example.test").await, "first backend");
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
            changed.validate().unwrap().normalize(self.first.clone()),
        )
        .await;
        assert!(self.store.routing_table().await.unwrap()[&self.first].is_empty());
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

#[tokio::test]
#[ignore = "requires the isolated Docker harness with loopback test ports"]
async fn ingress_caddy_routes_tls_network_changes_and_disable() {
    tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .unwrap();
    let scenario = Scenario::new().await;
    scenario.assert_public_routing().await;
    scenario
        .add_application_without_interrupting_traffic()
        .await;
    scenario.restart_without_daemon().await;
    scenario.repoint_after_backend_convergence().await;
    scenario.disable_and_restore_deployed_routes().await;
}
