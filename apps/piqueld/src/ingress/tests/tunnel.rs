//! Tunnel mode: `cloudflared`'s configuration and container, and the gateway's
//! tunnel listener. The harness test emulates `cloudflared` with a container
//! on the edge network, since a real tunnel needs Cloudflare.
use super::{FailingEngine, Scenario, dns01::CURL_IMAGE};
use crate::{
    config::{Credential, TunnelConfig, TunnelCredentials},
    ingress::Ingress,
    store::Store,
};
use piqueld_core::{EnvironmentId, manifest::Visibility};
use serde_json::{Value, json};
use std::sync::Arc;

const ID: &str = "6ff42ae2-765d-4adf-8112-31c55c1551ef";

/// `[ingress.tunnel]` with credentials holding `secret`.
fn tunnel(secret: &str) -> TunnelConfig {
    TunnelConfig {
        credentials: Some(TunnelCredentials {
            id: ID.parse().unwrap(),
            file: Credential::from(format!(
                r#"{{"AccountTag":"account","TunnelSecret":"{secret}","TunnelID":"{ID}"}}"#
            )),
        }),
    }
}

/// An ingress in tunnel mode whose Docker Engine is never called.
async fn tunnel_ingress(directory: &tempfile::TempDir) -> Ingress {
    let store = Arc::new(Store::open(directory.path().join("db")).await.unwrap());
    // Docker clients require the socket file to exist; nothing listens on it.
    let socket = directory.path().join("docker.sock");
    drop(tokio::net::UnixListener::bind(&socket).unwrap());
    Ingress::new(true, &socket, directory.path(), store)
        .unwrap()
        .with_tunnel(&tunnel("c2VjcmV0"))
}

#[tokio::test]
async fn cloudflared_forwards_everything_to_the_tunnel_listener_without_privileges() {
    let directory = tempfile::tempdir().unwrap();
    let ingress = tunnel_ingress(&directory).await;
    let gateway = &ingress.name;
    let credentials = ingress.tunnel.as_ref().unwrap();
    assert_eq!(
        ingress.tunnel_configuration(credentials),
        json!({
            "tunnel":ID,
            "credentials-file":"/etc/cloudflared/credentials.json",
            "metrics":"0.0.0.0:2000",
            "ingress":[{"service":format!("http://{gateway}:8080")}]
        })
    );
    let spec = ingress.tunnel_spec(credentials);
    assert_eq!(spec["Image"], super::super::CLOUDFLARED_IMAGE);
    assert_eq!(
        spec["Cmd"],
        json!(["tunnel", "--config", "/etc/cloudflared/config.json", "run"])
    );
    let host = &spec["HostConfig"];
    assert_eq!(host["NetworkMode"], json!(gateway));
    assert_eq!(host["CapDrop"], json!(["ALL"]));
    assert!(host.get("CapAdd").is_none());
    assert_eq!(host["ReadonlyRootfs"], true);
    assert_eq!(host["RestartPolicy"]["Name"], "unless-stopped");
    assert!(host.get("PortBindings").is_none());
    assert_eq!(
        host["Binds"],
        json!([format!(
            "{}:/etc/cloudflared:ro",
            directory.path().join("ingress/tunnel").display()
        )])
    );
    // New credentials replace the container, which reads them again.
    let hash = |secret: &str| {
        let config = tunnel(secret);
        ingress.tunnel_spec(config.credentials.as_ref().unwrap())["Labels"]
            ["io.piqueld.ingress-configuration"]
            .clone()
    };
    assert_eq!(
        hash("c2VjcmV0"),
        spec["Labels"]["io.piqueld.ingress-configuration"]
    );
    assert_ne!(hash("c2VjcmV0"), hash("bmV3"));
    // The gateway publishes no port at all in tunnel mode.
    let gateway = ingress.container_spec();
    assert!(gateway.get("ExposedPorts").is_none());
    assert!(gateway["HostConfig"].get("PortBindings").is_none());
}

#[tokio::test]
async fn the_tunnel_listener_serves_only_public_routes_to_edge_peers() {
    let directory = tempfile::tempdir().unwrap();
    let ingress =
        tunnel_ingress(&directory)
            .await
            .with_private(&crate::config::PrivateIngressConfig {
                enabled: true,
                ..Default::default()
            });
    let routes = |visibility: Visibility, host: &str| {
        let mut manifest = super::application("one", host, "body").to_manifest();
        manifest.spec.routes[0].visibility = visibility;
        manifest.validate().unwrap().spec().routes.clone()
    };
    let table: crate::store::ingress::RoutingTable = [
        (
            EnvironmentId::parse("env-public").unwrap(),
            routes(Visibility::Public, "www.example.com"),
        ),
        (
            EnvironmentId::parse("env-private").unwrap(),
            routes(Visibility::Private, "admin.example.com"),
        ),
    ]
    .into();
    let edge = ["172.20.0.0/16".to_owned()];
    let configuration = ingress.build_configuration(&table, Some(&edge), Some(&edge));
    let servers = &configuration["apps"]["http"]["servers"];
    // Ports 80 and 443 have no server; private routes keep their own.
    assert_eq!(
        servers.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["private", "private_http", "tunnel"]
    );
    let tunnel = &servers["tunnel"];
    assert_eq!(tunnel["listen"], json!([":8080"]));
    // Cloudflare terminates TLS, so Caddy never manages a certificate here.
    assert_eq!(tunnel["automatic_https"], json!({"disable":true}));
    let routes = tunnel["routes"].as_array().unwrap();
    // Applications on networks attached to the gateway are refused first.
    assert_eq!(
        routes[0],
        json!({"match":[{"not":[{"remote_ip":{"ranges":edge}}]}],"handle":[{"handler":"static_response","abort":true}],"terminal":true})
    );
    // The probe endpoint and the route itself, for the public host only.
    let hosts: Vec<&Value> = routes
        .iter()
        .filter_map(|route| route["match"][0].get("host"))
        .collect();
    assert_eq!(hosts, [&json!(["www.example.com"]); 2]);
    // Only edge peers set the client address, and backends see it.
    assert_eq!(
        tunnel["trusted_proxies"],
        json!({"source":"static","ranges":edge})
    );
    assert_eq!(tunnel["client_ip_headers"], json!(["Cf-Connecting-IP"]));
    let proxy = routes
        .iter()
        .find_map(|route| {
            route["handle"]
                .get(0)
                .filter(|handle| handle["handler"] == "reverse_proxy")
        })
        .unwrap();
    assert_eq!(
        proxy["headers"]["request"]["set"],
        json!({"X-Forwarded-For":["{http.vars.client_ip}"],"X-Forwarded-Proto":["https"]})
    );
    // Private routes' backends keep the PROXY header's client address.
    let private = servers["private"]["routes"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|route| {
            route["handle"]
                .get(0)
                .filter(|handle| handle["handler"] == "reverse_proxy")
        })
        .unwrap();
    assert!(private.get("headers").is_none());
}

/// Whether `ingress` may replace a running direct-mode gateway while an
/// application's retained routes lack a verified network.
async fn defers_replacing_a_direct_gateway(tunnel: Option<&TunnelConfig>) -> anyhow::Result<()> {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("docker.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let store = Arc::new(Store::open(directory.path().join("db")).await.unwrap());
    let container = json!({
        "Config":{"Labels":{
            "io.piqueld.managed":"true","io.piqueld.instance":store.instance_id(),"io.piqueld.ingress":"true"
        }},
        "State":{"Running":true},
        "HostConfig":{"PortBindings":{"443/tcp":[{"HostIp":"","HostPort":"443"}]}}
    });
    let engine = axum::Router::new().fallback(move || async move { axum::Json(container) });
    let server = tokio::spawn(async move { axum::serve(listener, engine).await });
    let mut ingress = Ingress::new(true, &socket, directory.path(), store).unwrap();
    if let Some(tunnel) = tunnel {
        ingress = ingress.with_tunnel(tunnel);
    }
    let routes = super::application("one", "www.example.com", "body")
        .to_manifest()
        .validate()
        .unwrap()
        .spec()
        .routes
        .clone();
    let table = [(EnvironmentId::parse("env-broken").unwrap(), routes)].into();
    let result = ingress
        .replace_gateway(
            &table,
            &std::collections::BTreeSet::new(),
            &ingress.container_spec(),
        )
        .await;
    server.abort();
    result
}

#[tokio::test]
async fn a_deferred_replacement_never_switches_modes() {
    defers_replacing_a_direct_gateway(None).await.unwrap();
    // The old gateway would keep ports 80/443 open in tunnel mode.
    let error = defers_replacing_a_direct_gateway(Some(&tunnel("c2VjcmV0")))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("cannot switch between direct and tunnel"),
        "{error:#}"
    );
}

#[tokio::test]
async fn disabled_ingress_removes_the_tunnel_credentials_even_if_docker_fails() {
    let engine = FailingEngine::start().await;
    let directory = engine.directory.path();
    let credentials = directory.join("ingress/tunnel/credentials.json");
    std::fs::create_dir_all(credentials.parent().unwrap()).unwrap();
    std::fs::write(&credentials, "{}").unwrap();
    let disabled = Ingress::new(
        false,
        &directory.join("docker.sock"),
        directory,
        Arc::clone(&engine.store),
    )
    .unwrap()
    .with_tunnel(&tunnel("c2VjcmV0"));
    disabled.synchronize().await.unwrap_err();
    assert!(!credentials.exists());
}

#[tokio::test]
async fn disabled_ingress_stops_containers_even_if_the_credentials_remain() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("docker.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let inspected = Arc::new(std::sync::Mutex::new(Vec::new()));
    let engine = axum::Router::new().fallback({
        let inspected = Arc::clone(&inspected);
        move |uri: axum::http::Uri| async move {
            inspected.lock().unwrap().push(uri.path().to_owned());
            axum::http::StatusCode::NOT_FOUND
        }
    });
    let server = tokio::spawn(async move { axum::serve(listener, engine).await });
    // A directory where the credentials file belongs cannot be removed.
    std::fs::create_dir_all(directory.path().join("ingress/tunnel/credentials.json/x")).unwrap();
    let store = Arc::new(Store::open(directory.path().join("db")).await.unwrap());
    let disabled = Ingress::new(false, &socket, directory.path(), store)
        .unwrap()
        .with_tunnel(&tunnel("c2VjcmV0"));
    let error = disabled.synchronize().await.unwrap_err();
    server.abort();
    assert!(
        format!("{error:#}").contains("remove the Cloudflare Tunnel credentials"),
        "{error:#}"
    );
    let gateway = format!("/containers/{}/json", disabled.name);
    assert!(
        inspected
            .lock()
            .unwrap()
            .iter()
            .any(|path| path.ends_with(&gateway))
    );
}

#[tokio::test]
#[ignore = "requires the isolated Docker harness with loopback test ports"]
async fn ingress_caddy_tunnel() {
    use futures_util::FutureExt;
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let scenario = Box::pin(Scenario::new()).await;
    let result =
        Box::pin(std::panic::AssertUnwindSafe(scenario.tunnel_mode()).catch_unwind()).await;
    scenario.gateway.stop_gateway().await.unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

impl Scenario {
    /// Switches the scenario's gateway to tunnel mode and back.
    async fn tunnel_mode(&self) {
        let echo = super::deploy(
            &self.store,
            &self.controller,
            super::application(
                "echo",
                "echo.example.test",
                "{http.request.header.X-Forwarded-For} {http.request.header.X-Forwarded-Proto}",
            ),
        )
        .await;
        let mut admin = super::application("admin", "admin.example.test", "admin").to_manifest();
        admin.spec.routes[0].visibility = Visibility::Private;
        let admin = admin
            .validate()
            .unwrap()
            .normalize(piqueld_core::ApplicationId::parse("admin-input").unwrap());
        super::deploy(&self.store, &self.controller, admin).await;
        let ingress = Ingress::new(
            true,
            &self.socket,
            self.directory.path(),
            Arc::clone(&self.store),
        )
        .unwrap()
        .with_tunnel(&tunnel("c2VjcmV0"));
        ingress.synchronize().await.unwrap();
        self.tunnel_publishes_no_ports(&ingress).await;
        self.only_cloudflared_reaches_the_tunnel_listener(&ingress, &echo)
            .await;

        // Back to direct mode: cloudflared and its credentials are removed.
        self.gateway.synchronize().await.unwrap();
        assert!(
            self.gateway
                .named_container(&ingress.tunnel_name())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !self
                .directory
                .path()
                .join("ingress/tunnel/credentials.json")
                .exists()
        );
        assert_eq!(self.body("one.example.test").await, "first backend");
    }

    /// Neither the gateway nor `cloudflared` publishes a port, so the
    /// harness's published ports 80/443 lead nowhere.
    async fn tunnel_publishes_no_ports(&self, ingress: &Ingress) {
        for name in [ingress.name.clone(), ingress.tunnel_name()] {
            let container = ingress.named_container(&name).await.unwrap().unwrap();
            let bindings = &container["HostConfig"]["PortBindings"];
            assert!(
                bindings.is_null() || bindings.as_object().is_some_and(serde_json::Map::is_empty),
                "{name} publishes {bindings}"
            );
        }
        let cloudflared = ingress
            .named_container(&ingress.tunnel_name())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cloudflared["HostConfig"]["ReadonlyRootfs"], true);
        assert!(
            self.client
                .get(self.url("one.example.test"))
                .send()
                .await
                .is_err()
        );
        // Without a real tunnel, cloudflared never connects.
        assert!(!ingress.status().await.healthy);
    }

    /// From the edge network, as `cloudflared`, public routes answer with
    /// `Cf-Connecting-IP` as the client address and private routes do not
    /// exist. From an application's network, the listener refuses even public
    /// routes, so no forged `Cf-Connecting-IP` reaches a backend.
    async fn only_cloudflared_reaches_the_tunnel_listener(
        &self,
        ingress: &Ingress,
        echo: &EnvironmentId,
    ) {
        self.pull(CURL_IMAGE).await;
        let request = |gateway: &str, host: &str| {
            [
                "-s",
                "--max-time",
                "5",
                "-w",
                " %{http_code}",
                "-H",
                &format!("Host: {host}"),
                "-H",
                "Cf-Connecting-IP: 203.0.113.7",
                "-H",
                "X-Forwarded-For: 198.51.100.1",
                &format!("http://{gateway}:8080/"),
            ]
            .map(str::to_owned)
            .to_vec()
        };
        let (code, output) = self
            .curl(&ingress.name, &request(&ingress.name, "echo.example.test"))
            .await;
        assert_eq!((code, output.as_str()), (0, "203.0.113.7 https 200"));
        // The private route is deployed, but never served here.
        let (code, output) = self
            .curl(&ingress.name, &request(&ingress.name, "admin.example.test"))
            .await;
        assert_eq!((code, output.as_str()), (0, " 404"));

        let network = piqueld_core::DockerNetworkName::for_ingress(echo).to_string();
        let gateway = ingress.container().await.unwrap().unwrap();
        let address = gateway["NetworkSettings"]["Networks"][&network]["IPAddress"]
            .as_str()
            .unwrap()
            .to_owned();
        let (code, output) = self
            .curl(&network, &request(&address, "echo.example.test"))
            .await;
        assert_ne!(
            code, 0,
            "an application reached the tunnel listener: {output}"
        );
    }
}
