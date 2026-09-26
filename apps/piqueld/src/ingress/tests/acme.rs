//! Real ACME orders and challenges against Caddy's bundled test CA, with short
//! certificates so renewal is exercised without public DNS or CA rate limits.
use super::Scenario;
use hyper::Method;
use serde_json::json;
use std::time::Duration;

impl Scenario {
    async fn configure_acme_test(&self) -> serde_json::Value {
        let hosts = ["http-acme.example.test", "alpn-acme.example.test"];
        // Docker's test-only ExtraHosts resolve challenges to the gateway itself.
        let original = self
            .gateway
            .caddy
            .json(Method::GET, "/config/", None)
            .await
            .unwrap();
        let mut table = self.store.routing_table().await.unwrap();
        for host in hosts {
            let mut route = table[&self.first][0].clone();
            route.hostname = super::application("acme", host, "unused").spec().routes[0]
                .hostname
                .clone();
            table.get_mut(&self.first).unwrap().push(route);
        }
        let mut configuration = self.gateway.configuration(&table);
        configuration["apps"]["http"]["servers"]["acme"] = json!({
            "listen":["127.0.0.1:9072"], "tls_connection_policies":[{}],
            "routes":[{"match":[{"host":["127.0.0.1"]}], "handle":[{"handler":"acme_server", "lifetime":300_000_000_000_u64,
                "challenges":["http-01", "tls-alpn-01"]}]}]
        });
        // Provision the directory's own TLS certificate before ACME clients use it.
        let mut directory = original.clone();
        directory["apps"]["http"]["servers"]["acme"] =
            configuration["apps"]["http"]["servers"]["acme"].clone();
        self.gateway
            .caddy
            .json(Method::POST, "/load", Some(&directory))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            let certificate = self
                .directory
                .path()
                .join("ingress/data/caddy/certificates/local/127.0.0.1/127.0.0.1.crt");
            while !tokio::fs::try_exists(&certificate).await.unwrap() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("ACME directory has its own trusted TLS certificate");
        let policy = |host: &str, disabled: &str| {
            json!({
                "subjects":[host], "renewal_window_ratio":0.95,
                "issuers":[{"module":"acme", "ca":"https://127.0.0.1:9072/acme/local/directory",
                "trusted_roots_pem_files":["/data/caddy/pki/authorities/local/root.crt"],
                    "challenges":{disabled:{"disabled":true}}}]
            })
        };
        configuration["apps"]["tls"]["automation"] = json!({
            "renew_interval":"1s",
            "policies":[policy(hosts[0], "tls-alpn"), policy(hosts[1], "http"),
                {"issuers":[{"module":"internal"}]}]
        });
        self.gateway
            .caddy
            .json(Method::POST, "/load", Some(&configuration))
            .await
            .unwrap();
        // CertMagic's maintenance ticker is created at process startup. Restart
        // from the saved test config so its accelerated renewal interval takes effect.
        self.gateway
            .docker
            .json(
                Method::POST,
                &format!("/containers/{}/restart?t=1", self.gateway.name),
                None,
            )
            .await
            .unwrap();
        original
    }

    pub(super) async fn acme_challenges_and_renewal(&self) {
        let hosts = ["http-acme.example.test", "alpn-acme.example.test"];
        let original = self.configure_acme_test().await;
        let root = tokio::fs::read(
            self.directory
                .path()
                .join("ingress/data/caddy/pki/authorities/local/root.crt"),
        )
        .await
        .unwrap();
        // A new client also discards TLS session tickets. Merely disabling the
        // HTTP pool can resume an old TLS session and report its old certificate.
        let client = || {
            reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(3))
                .tls_info(true)
                .pool_max_idle_per_host(0)
                .add_root_certificate(reqwest::Certificate::from_pem(&root).unwrap())
                .resolve(hosts[0], "127.0.0.1:443".parse().unwrap())
                .resolve(hosts[1], "127.0.0.1:443".parse().unwrap())
                .build()
                .unwrap()
        };
        for host in hosts {
            let first = tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    if let Ok(response) = client()
                        .get(format!(
                            "{}{}",
                            self.url(host),
                            ".well-known/piqueld-ingress"
                        ))
                        .send()
                        .await
                    {
                        let certificate = response
                            .extensions()
                            .get::<reqwest::tls::TlsInfo>()
                            .unwrap()
                            .peer_certificate()
                            .unwrap()
                            .to_vec();
                        assert_eq!(response.text().await.unwrap(), self.gateway.instance_id);
                        break certificate;
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            })
            .await
            .expect("ACME challenge produces a trusted certificate");
            // No daemon reconciliation/reload occurs here: Caddy must renew by
            // itself, and a fresh TLS handshake must serve the renewed certificate.
            tokio::time::timeout(Duration::from_secs(90), async {
                loop {
                    let response = client()
                        .get(format!(
                            "{}{}",
                            self.url(host),
                            ".well-known/piqueld-ingress"
                        ))
                        .send()
                        .await
                        .unwrap();
                    let certificate = response
                        .extensions()
                        .get::<reqwest::tls::TlsInfo>()
                        .unwrap()
                        .peer_certificate()
                        .unwrap()
                        .to_vec();
                    assert_eq!(response.text().await.unwrap(), self.gateway.instance_id);
                    if certificate != first {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            })
            .await
            .expect("ACME certificate renews and is served without daemon intervention");
        }
        self.gateway
            .caddy
            .json(Method::POST, "/load", Some(&original))
            .await
            .unwrap();
    }
}
