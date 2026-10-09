//! DNS-01 issuance against Pebble, with TXT records served by `challtestsrv`.
//! Both run inside the isolated Docker daemon; the harness publishes their ports.
//! The certificates serve a private route on the private listener, which this
//! test reaches through a relay adding a PROXY header, as the apps node does.
use super::Scenario;
use crate::{
    config::AcmeConfig,
    dns::{Dns, DnsProvider, Record, challtestsrv::Challtestsrv},
    ingress::{
        Ingress,
        certificates::{CertificateName, Certificates, Challenge},
        node::proxy_relay,
    },
};
use hickory_resolver::{
    Resolver,
    config::{ConnectionConfig, NameServerConfig, ResolverConfig},
    net::runtime::TokioRuntimeProvider,
};
use hyper::Method;
use piqueld_core::{event::Event, manifest::Hostname, observability::EventFilter};
use serde_json::json;
use std::{collections::BTreeSet, net::SocketAddr, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

const PEBBLE_IMAGE: &str = "ghcr.io/letsencrypt/pebble:2.10.1@sha256:ddf230642b1a584f519f32e347de1b05a6e4c1f6c35c1863b33effeab5f78199";
pub(super) const CURL_IMAGE: &str =
    "alpine/curl:8.22.0@sha256:3f21f10cf24835f7baae20931f18640cc30915ea4151363fe4d8fb59ee296dfb";
const CHALLTESTSRV_IMAGE: &str = "ghcr.io/letsencrypt/pebble-challtestsrv:2.10.1@sha256:12ce21884def456bcf9786542113949e1f19dc7738d2c70e156c2d0c38a1405b";

fn port(variable: &str) -> u16 {
    std::env::var(variable).unwrap().parse().unwrap()
}

fn hosts(names: &[&str]) -> BTreeSet<Hostname> {
    names
        .iter()
        .map(|name| Hostname::parse(*name).unwrap())
        .collect()
}

impl Scenario {
    /// Pulls an image into the isolated daemon.
    pub(super) async fn pull(&self, image: &str) {
        use bollard::query_parameters::CreateImageOptionsBuilder;
        use futures_util::TryStreamExt;
        let options = CreateImageOptionsBuilder::default()
            .from_image(image)
            .build();
        self.gateway
            .images
            .create_image(Some(options), None, None)
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
    }

    /// Starts `challtestsrv` and Pebble on a shared bridge network and returns
    /// Pebble's test root CA, which signs its ACME API certificate.
    async fn start_pebble(&self) -> Vec<u8> {
        let docker = &self.gateway.docker;
        docker
            .external(
                Method::POST,
                "/networks/create",
                Some(&json!({"Name":"pebble"})),
            )
            .await
            .unwrap();
        for (name, image, command, ports, env) in [
            (
                "challtestsrv",
                CHALLTESTSRV_IMAGE,
                json!([
                    "-dnsserver",
                    ":8053",
                    "-management",
                    ":8055",
                    "-http01",
                    "",
                    "-https01",
                    "",
                    "-tlsalpn01",
                    "",
                    "-doh",
                    ""
                ]),
                json!({"8053/tcp":[{"HostPort":"8053"}],"8055/tcp":[{"HostPort":"8055"}]}),
                json!([]),
            ),
            (
                "pebble",
                PEBBLE_IMAGE,
                json!([
                    "-config",
                    "test/config/pebble-config.json",
                    "-dnsserver",
                    "challtestsrv:8053"
                ]),
                json!({"14000/tcp":[{"HostPort":"14000"}]}),
                json!(["PEBBLE_VA_NOSLEEP=1", "PEBBLE_WFE_NONCEREJECT=0"]),
            ),
        ] {
            self.pull(image).await;
            docker
                .external(
                    Method::POST,
                    &format!("/containers/create?name={name}"),
                    Some(&json!({
                        "Image":image,"Cmd":command,"Env":env,
                        "HostConfig":{"NetworkMode":"pebble","PortBindings":ports}
                    })),
                )
                .await
                .unwrap();
            docker
                .external(Method::POST, &format!("/containers/{name}/start"), None)
                .await
                .unwrap();
        }
        // `/archive` answers with a tar stream: a 512-byte header whose octal
        // size field starts at byte 124, then the file.
        let archive = docker
            .read("/containers/pebble/archive?path=/test/certs/pebble.minica.pem")
            .await
            .unwrap();
        let size = usize::from_str_radix(
            std::str::from_utf8(&archive[124..136])
                .unwrap()
                .trim_matches(['\0', ' ']),
            8,
        )
        .unwrap();
        archive[512..512 + size].to_vec()
    }

    /// Removes the Pebble containers and network.
    pub(super) async fn stop_pebble(&self) {
        for path in [
            "/containers/pebble?force=true",
            "/containers/challtestsrv?force=true",
            "/networks/pebble",
        ] {
            let _ = self
                .gateway
                .docker
                .external(Method::DELETE, path, None)
                .await;
        }
    }

    /// A second manager of this scenario's gateway, issuing from Pebble.
    fn pebble_ingress(&self, root: &std::path::Path) -> Ingress {
        let mut dns = Dns::new(vec![
            DnsProvider::Challtestsrv(Challtestsrv {
                management: format!("http://127.0.0.1:{}", port("PIQUELD_CHALLTESTSRV_API_PORT")),
                zones: hosts(&["example.test"]).into_iter().collect(),
            })
            .into(),
        ])
        .unwrap();
        // One nameserver whose first address refuses connections, like an
        // IPv6 address on an IPv4-only host: its second address answers.
        dns.nameservers = Some(vec![vec![
            SocketAddr::from(([127, 0, 0, 1], 1)),
            SocketAddr::from(([127, 0, 0, 1], port("PIQUELD_CHALLTESTSRV_DNS_PORT"))),
        ]]);
        let mut ingress = Ingress::new(
            true,
            &self.socket,
            self.directory.path(),
            Arc::clone(&self.store),
        )
        .unwrap()
        .with_private(&crate::config::PrivateIngressConfig {
            enabled: true,
            ..Default::default()
        });
        // Same container spec and issuer as the scenario's gateway.
        ingress.issuer.clone_from(&self.gateway.issuer);
        ingress.extra_hosts.clone_from(&self.gateway.extra_hosts);
        ingress.private_port = self.gateway.private_port;
        ingress.certificates = Certificates::new(
            dns,
            AcmeConfig {
                directory: format!("https://127.0.0.1:{}/dir", port("PIQUELD_PEBBLE_PORT")),
                email: Some("admin@example.test".into()),
            },
            ingress.directory.clone(),
        );
        ingress.certificates.acme_root = Some(root.to_owned());
        ingress.certificates.propagation_timeout = Duration::from_secs(3);
        ingress
    }

    /// TXT values `challtestsrv` serves at `name`.
    async fn txt(&self, name: &str) -> Vec<String> {
        let mut connection = ConnectionConfig::tcp();
        connection.port = port("PIQUELD_CHALLTESTSRV_DNS_PORT");
        let resolver = Resolver::builder_with_config(
            ResolverConfig::from_parts(
                None,
                Vec::new(),
                vec![NameServerConfig::new(
                    [127, 0, 0, 1].into(),
                    true,
                    vec![connection],
                )],
            ),
            TokioRuntimeProvider::default(),
        )
        .build()
        .unwrap();
        match resolver.txt_lookup(format!("{name}.")).await {
            Ok(lookup) => lookup
                .answers()
                .iter()
                .map(|record| record.data.to_string())
                .collect(),
            Err(error) if error.is_no_records_found() => Vec::new(),
            Err(error) => panic!("query challtestsrv: {error}"),
        }
    }

    /// Requests `https://<hostname>/` from the listener on loopback `port`
    /// without verifying its certificate.
    async fn untrusted(&self, hostname: &str, port: u16) -> reqwest::Result<reqwest::Response> {
        reqwest::Client::builder()
            .no_proxy()
            .danger_accept_invalid_certs(true)
            .tls_info(true)
            .resolve(hostname, SocketAddr::from(([127, 0, 0, 1], port)))
            .build()
            .unwrap()
            .get(format!("https://{hostname}:{port}/"))
            .send()
            .await
    }

    /// A loopback port relaying to the private listener with a PROXY header
    /// naming a tailnet client, like the apps node.
    async fn relay(&self) -> (u16, tokio::task::JoinSet<()>) {
        let private = SocketAddr::from(([127, 0, 0, 1], self.gateway.private_port.unwrap()));
        let (relay, task) = proxy_relay(private, "100.64.0.9:41000".parse().unwrap())
            .await
            .unwrap();
        (relay.port(), task)
    }

    /// The certificate the private listener presents for `hostname`.
    async fn served_certificate(&self, hostname: &str) -> Vec<u8> {
        let (relay, _relay) = self.relay().await;
        let response = self.untrusted(hostname, relay).await.unwrap();
        response
            .extensions()
            .get::<reqwest::tls::TlsInfo>()
            .unwrap()
            .peer_certificate()
            .unwrap()
            .to_vec()
    }

    /// Runs `curl` on `network` and returns its exit code and output.
    pub(super) async fn curl(&self, network: &str, arguments: &[String]) -> (i64, String) {
        let docker = &self.gateway.docker;
        let name = "piqueld-test-curl";
        docker
            .external(
                Method::POST,
                &format!("/containers/create?name={name}"),
                Some(&json!({"Image":CURL_IMAGE,"Cmd":arguments,"HostConfig":{"NetworkMode":network}})),
            )
            .await
            .unwrap();
        docker
            .external(Method::POST, &format!("/containers/{name}/start"), None)
            .await
            .unwrap();
        let exited = docker
            .external(Method::POST, &format!("/containers/{name}/wait"), None)
            .await
            .unwrap();
        let output = self
            .gateway
            .container_logs(name, "tail=100")
            .await
            .unwrap()
            .join("\n");
        docker
            .external(
                Method::DELETE,
                &format!("/containers/{name}?force=true"),
                None,
            )
            .await
            .unwrap();
        (exited["StatusCode"].as_i64().unwrap(), output)
    }

    /// Applications reach the gateway's private listener over their ingress
    /// network, but complete TLS for a private route neither directly nor
    /// with a forged PROXY header. Public routes stay reachable as a control.
    async fn applications_cannot_reach_private_routes(&self, admin: &piqueld_core::EnvironmentId) {
        let network = piqueld_core::DockerNetworkName::for_ingress(admin).to_string();
        let gateway = self.gateway.container().await.unwrap().unwrap();
        let address = gateway["NetworkSettings"]["Networks"][&network]["IPAddress"]
            .as_str()
            .unwrap()
            .to_owned();
        self.pull(CURL_IMAGE).await;
        let request = |host: &str, port: u16| {
            vec![
                "-sk".to_owned(),
                "--max-time".into(),
                "5".into(),
                "--resolve".into(),
                format!("{host}:{port}:{address}"),
                format!("https://{host}:{port}/"),
            ]
        };
        assert_eq!(
            self.curl(&network, &request("one.example.test", 443))
                .await
                .0,
            0
        );
        assert_ne!(
            self.curl(&network, &request("admin.example.test", 8443))
                .await
                .0,
            0
        );
        let mut forged = request("admin.example.test", 8443);
        forged.extend(["--haproxy-protocol", "--haproxy-clientip", "100.64.0.9"].map(String::from));
        assert_ne!(self.curl(&network, &forged).await.0, 0);
    }

    /// Deploys `admin.example.test` publicly, then makes it private: the
    /// change withdraws it from the public listener.
    async fn deploy_private_route(&self) -> piqueld_core::EnvironmentId {
        let admin = |visibility: &str| {
            let mut manifest = super::application(
                "admin",
                "admin.example.test",
                "{http.request.header.X-Forwarded-For}",
            )
            .to_manifest();
            manifest.spec.routes[0].visibility = visibility.parse().unwrap();
            manifest
                .validate()
                .unwrap()
                .normalize(piqueld_core::ApplicationId::parse("admin-input").unwrap())
        };
        let id = super::deploy(&self.store, &self.controller, admin("public")).await;
        let public = |scheme: &str, port: u16| {
            self.client
                .get(format!("{scheme}://127.0.0.1:{port}/"))
                .header("Host", "admin.example.test")
                .send()
        };
        assert_eq!(public("http", self.plain_port).await.unwrap().status(), 308);
        let deployed =
            admin("private").with_id(piqueld_core::ApplicationId::parse(id.as_str()).unwrap());
        super::deploy(&self.store, &self.controller, deployed).await;
        // A forged Host gets a 404, and its SNI no certificate.
        assert_eq!(public("http", self.plain_port).await.unwrap().status(), 404);
        assert!(
            self.untrusted("admin.example.test", self.tls_port)
                .await
                .is_err()
        );
        id
    }

    /// The private listener serves only private routes, with their DNS-01
    /// certificate, and backends see the PROXY header's client address.
    async fn private_listener_serves_private_routes(&self, ingress: &Ingress) {
        let private = self.gateway.private_port.unwrap();
        assert!(
            self.untrusted("one.example.test", private).await.is_err(),
            "a public route completed TLS on the private listener"
        );
        let (relay, _relay) = self.relay().await;
        let body = self
            .untrusted("admin.example.test", relay)
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "100.64.0.9");
        // The apps node runs hardened beside the gateway and answers on its
        // LocalAPI, logged in or not.
        let node = ingress
            .named_container(&ingress.node_name())
            .await
            .unwrap()
            .expect("the apps node runs");
        assert_eq!(node["HostConfig"]["ReadonlyRootfs"], true);
        tokio::time::timeout(Duration::from_mins(1), async {
            loop {
                ingress.synchronize().await.unwrap();
                if !ingress.status().await.private.state.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        })
        .await
        .expect("the apps node reports its state");
    }

    /// The newest 100 `kind` events, filtered to journal `phase`.
    async fn actions(&self, kind: &str, phase: &str) -> Vec<Event> {
        let filter = EventFilter {
            kind: Some(kind.into()),
            descending: true,
            ..EventFilter::default()
        };
        let mut events = self
            .store
            .filtered_events(&filter, &crate::store::Visibility::ALL, None, 100)
            .await
            .unwrap()
            .items;
        events.retain(|e| e.phase.as_deref() == Some(phase));
        events
    }

    /// The leaf certificate stored in file `name`, after its private key.
    async fn stored_leaf(&self, name: &str) -> Vec<u8> {
        let bundle = tokio::fs::read_to_string(
            self.directory
                .path()
                .join("ingress/certificates")
                .join(name),
        )
        .await
        .unwrap();
        let chain = &bundle[bundle.find("-----BEGIN CERTIFICATE-----").unwrap()..];
        x509_parser::pem::parse_x509_pem(chain.as_bytes())
            .unwrap()
            .1
            .contents
    }

    pub(super) async fn dns01_certificates(&self) {
        let root = self.start_pebble().await;
        let root_path = self.directory.path().join("pebble.minica.pem");
        tokio::fs::write(&root_path, &root).await.unwrap();
        let ingress = self.pebble_ingress(&root_path);
        let admin = self.deploy_private_route().await;
        let desired = hosts(&["admin.example.test", "www.example.test", "example.test"]);
        let now = crate::store::now_ms();
        let expires = self.dns01_issue(&ingress, &desired, now).await;
        self.private_listener_serves_private_routes(&ingress).await;
        self.applications_cannot_reach_private_routes(&admin).await;
        self.dns01_renew(&ingress, &desired, expires).await;
        self.dns01_failure(&ingress, now).await;
        self.dns01_cancel(&root_path, now).await;
        self.dns01_leftover(&ingress, now).await;
        self.dns01_retire(&ingress, now, expires).await;
        // Restore the scenario gateway's own configuration.
        ingress.synchronize().await.unwrap();
    }

    /// Two children share the zone's wildcard and the apex gets an exact name;
    /// the private listener serves them. Returns the latest expiry.
    async fn dns01_issue(&self, ingress: &Ingress, desired: &BTreeSet<Hostname>, now: i64) -> i64 {
        use std::os::unix::fs::PermissionsExt;
        tokio::time::timeout(
            Duration::from_mins(3),
            ingress.maintain_certificates(desired, now, &CancellationToken::new()),
        )
        .await
        .expect("issuance finishes");
        let status = ingress.dns_status().await;
        assert_eq!(status.providers[0].zones, ["example.test"]);
        let names: Vec<_> = status
            .certificates
            .iter()
            .map(|c| (c.name.as_str(), c.hostnames.len(), c.error.as_deref()))
            .collect();
        assert_eq!(
            names,
            [("*.example.test", 2, None), ("example.test", 1, None)]
        );
        assert_eq!(
            self.txt("_acme-challenge.example.test").await,
            [] as [String; 0]
        );
        for file in ["_wildcard.example.test.pem", "example.test.pem"] {
            let path = self
                .directory
                .path()
                .join("ingress/certificates")
                .join(file);
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        ingress.synchronize().await.unwrap();
        assert_eq!(
            self.served_certificate("admin.example.test").await,
            self.stored_leaf("_wildcard.example.test.pem").await
        );
        // Pebble picks a random profile per order, so lifetimes differ.
        status
            .certificates
            .iter()
            .map(|c| c.expires_at_ms.unwrap())
            .max()
            .unwrap()
    }

    /// With a third of the lifetime left both are renewed, and nothing else.
    async fn dns01_renew(&self, ingress: &Ingress, desired: &BTreeSet<Hostname>, expires: i64) {
        let stored = self.stored_leaf("_wildcard.example.test.pem").await;
        ingress
            .maintain_certificates(desired, expires - 86_400_000, &CancellationToken::new())
            .await;
        assert_eq!(
            self.actions("action_succeeded", "ingress_certificate_renew")
                .await
                .len(),
            2
        );
        assert_ne!(self.stored_leaf("_wildcard.example.test.pem").await, stored);
    }

    /// A record that never becomes visible fails issuance after it was
    /// created: it is still deleted, the error is reported, and the next
    /// attempt waits for the backoff.
    async fn dns01_failure(&self, ingress: &Ingress, now: i64) {
        let DnsProvider::Challtestsrv(provider) = ingress.certificates.dns.provider(0) else {
            unreachable!()
        };
        let http = ingress.certificates.dns.http();
        provider
            .servfail(http, "_acme-challenge.fail.example.test", true)
            .await;
        let failing = hosts(&["x.fail.example.test"]);
        ingress
            .maintain_certificates(&failing, now, &CancellationToken::new())
            .await;
        provider
            .servfail(http, "_acme-challenge.fail.example.test", false)
            .await;
        let status = ingress.dns_status().await;
        let failed = status
            .certificates
            .iter()
            .find(|c| c.name == "*.fail.example.test")
            .unwrap();
        assert!(
            failed.error.as_deref().unwrap().contains("was not visible"),
            "{failed:?}"
        );
        assert_eq!(
            self.txt("_acme-challenge.fail.example.test").await,
            [] as [String; 0]
        );
        assert!(
            self.actions("action_failed", "ingress_certificate_issue")
                .await
                .iter()
                .any(|e| e.error_code.as_deref() == Some("certificate_renewal_failed"))
        );
        let attempts = self
            .actions("action_started", "ingress_certificate_issue")
            .await
            .len();
        ingress
            .maintain_certificates(&failing, now + 1000, &CancellationToken::new())
            .await;
        assert_eq!(
            self.actions("action_started", "ingress_certificate_issue")
                .await
                .len(),
            attempts
        );
    }

    /// Shutdown while the order waits for propagation, after the TXT record
    /// was created, interrupts it and the record is still deleted. This
    /// manager's only nameserver refuses connections, so the record never
    /// propagates and the wait lasts until shutdown.
    async fn dns01_cancel(&self, root: &std::path::Path, now: i64) {
        let mut ingress = self.pebble_ingress(root);
        ingress.certificates.dns.nameservers =
            Some(vec![vec![SocketAddr::from(([127, 0, 0, 1], 1))]]);
        let shutdown = CancellationToken::new();
        let cancelled = hosts(&["x.cancel.example.test"]);
        tokio::join!(
            ingress.maintain_certificates(&cancelled, now, &shutdown),
            async {
                while self
                    .txt("_acme-challenge.cancel.example.test")
                    .await
                    .is_empty()
                {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                shutdown.cancel();
            }
        );
        let status = ingress.dns_status().await;
        let interrupted = status
            .certificates
            .iter()
            .find(|c| c.name == "*.cancel.example.test")
            .unwrap();
        assert!(
            interrupted
                .error
                .as_deref()
                .unwrap()
                .contains("cancelled by daemon shutdown"),
            "{interrupted:?}"
        );
        assert_eq!(
            self.txt("_acme-challenge.cancel.example.test").await,
            [] as [String; 0]
        );
    }

    /// A challenge record whose deletion failed earlier is deleted before
    /// the next attempt creates its own. `challtestsrv` clears every value at
    /// a name, so deleting it afterwards would also remove the new record and
    /// fail validation.
    async fn dns01_leftover(&self, ingress: &Ingress, now: i64) {
        let dns = &ingress.certificates.dns;
        let hostname = Hostname::parse("x.leftover.example.test").unwrap();
        let (provider, zone) = dns.zone_for(&hostname).await.unwrap();
        let id = dns
            .provider(provider)
            .upsert(
                dns.http(),
                &zone,
                "_acme-challenge.leftover.example.test",
                None,
                &Record::Txt("stale".into()),
            )
            .await
            .unwrap();
        ingress.certificates.replace_leftover(
            &CertificateName::Wildcard(hostname.parent().unwrap()),
            Some(Challenge { provider, zone, id }),
        );
        ingress
            .maintain_certificates(&[hostname].into(), now, &CancellationToken::new())
            .await;
        let status = ingress.dns_status().await;
        let issued = status
            .certificates
            .iter()
            .find(|c| c.name == "*.leftover.example.test")
            .unwrap();
        assert!(
            issued.error.is_none() && issued.expires_at_ms.is_some(),
            "{issued:?}"
        );
        assert_eq!(
            self.txt("_acme-challenge.leftover.example.test").await,
            [] as [String; 0]
        );
    }

    /// Unneeded certificates are kept until they expire, then deleted.
    async fn dns01_retire(&self, ingress: &Ingress, now: i64, expires: i64) {
        ingress
            .maintain_certificates(&BTreeSet::new(), now, &CancellationToken::new())
            .await;
        assert_eq!(ingress.dns_status().await.certificates.len(), 3);
        ingress
            .maintain_certificates(
                &BTreeSet::new(),
                expires + 365 * 86_400_000,
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(ingress.dns_status().await.certificates, []);
        assert!(
            !self
                .directory
                .path()
                .join("ingress/certificates/example.test.pem")
                .exists()
        );
    }
}
