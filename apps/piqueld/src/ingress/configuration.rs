//! Builds Caddy's runtime JSON from the deployed routing table. The daemon loads
//! it through Caddy's private Unix admin socket and persists it as the gateway's
//! autosave configuration for independent container restarts.
//!
//! Each visibility has its own listener, with its own route list:
//!
//! | Server | Listens on | Serves |
//! | --- | --- | --- |
//! | `public`, `public_http` | `:443`, `:80` (published) | public routes |
//! | `private`, `private_http` | `:8443`, `:8081` (not published) | private routes |
//!
//! A hostname appears only on its own listener, and each HTTPS server's TLS
//! policy accepts only its own hostnames, so a forged Host or SNI for a
//! private route on the public listener gets no certificate and a 404. The
//! private servers exist only while private ingress is enabled, and never fall
//! back to the public listener. Applications can connect to them too, since
//! their networks are attached to the gateway, so the private servers serve
//! only tailnet client addresses. Only edge network peers, the apps node among
//! them, may set the client address with a PROXY header; anyone else's header
//! is ignored and their own address is used.
//!
//! Known hosts redirect HTTP to HTTPS, then either proxy to their Swarm service's
//! internal HTTP port or answer with their configured redirect. Caddy
//! obtains/renews certificates for public hosts automatically. Private hosts use
//! the DNS-01 certificates piqueld loads as PEM, so automatic HTTPS is off on
//! the private servers. Explicit redirects keep unknown HTTP hosts on the final
//! 404 handler.

use super::{
    Ingress,
    node::{PRIVATE_HTTP_PORT, PRIVATE_HTTPS_PORT, TAILNET_RANGES},
};
use crate::store::ingress::RoutingTable;
use anyhow::Result;
use piqueld_core::{
    DockerServiceName,
    manifest::{RouteTarget, Visibility},
};
use serde_json::{Value, json};

/// The routes of one listener, as Caddy route objects.
#[derive(Default)]
struct Listener {
    /// HTTPS routes: each host's probe endpoint and destination.
    https: Vec<Value>,
    /// HTTP routes redirecting each host to HTTPS.
    redirects: Vec<Value>,
    /// Hostnames, which the TLS connection policy accepts as SNI.
    hosts: Vec<String>,
}

impl Ingress {
    /// The complete replacement configuration for `table`, with the private
    /// listener trusting PROXY headers from the edge network's subnets. The
    /// private listener is left out unless Swarm's address pools are verified
    /// outside the tailnet ranges.
    pub(super) async fn configuration(&self, table: &RoutingTable) -> Result<Value> {
        let proxies = match &self.node {
            Some(_) if self.check_tailnet_pools().await.is_ok() => Some(self.edge_subnets().await?),
            _ => None,
        };
        // Tests outside the engine reach the published private listener from
        // the host's Docker bridge, outside the edge network. Application
        // networks, from Swarm's 10.0.0.0/8 pool, stay untrusted.
        #[cfg(test)]
        let proxies = proxies.map(|mut proxies| {
            if self.private_port.is_some() {
                proxies.push("172.16.0.0/12".into());
            }
            proxies
        });
        Ok(self.build_configuration(table, proxies.as_deref()))
    }

    /// Produces the configuration. Private routes are served only when
    /// `proxies` gives the private listener's trusted PROXY sources. Exact-host
    /// routes enable automatic TLS without on-demand certificate issuance for
    /// arbitrary hosts.
    ///
    /// Each route yields a probe handler, a reverse proxy, and an HTTP redirect
    /// on its listener; every server ends with a 404 fallback.
    ///
    /// ```text
    /// app.example.com -> reverse_proxy <swarm service name>:<port>
    /// http://app.example.com/x -> 308 https://app.example.com/x
    /// ```
    pub(super) fn build_configuration(
        &self,
        table: &RoutingTable,
        proxies: Option<&[String]>,
    ) -> Value {
        let mut public = Listener::default();
        let mut private = Listener::default();
        for (id, routes) in table {
            for route in routes {
                let listener = match route.visibility {
                    Visibility::Public => &mut public,
                    Visibility::Private if proxies.is_some() => &mut private,
                    Visibility::Private => continue,
                };
                let host = route.hostname.as_str();
                listener.hosts.push(host.to_owned());
                // A bounded, side-effect-free endpoint lets the daemon distinguish
                // this gateway from a different server behind an incorrect DNS record.
                listener.https.push(json!({"match":[{"host":[host],"path":["/.well-known/piqueld-ingress"]}],"handle":[{"handler":"static_response","body":self.instance_id}],"terminal":true}));
                let handler = match &route.target {
                    RouteTarget::Service { service, port } => json!({
                        "handler":"reverse_proxy",
                        "upstreams":[{"dial":format!("{}:{port}", DockerServiceName::for_service(id,service))}],
                        "transport":{"protocol":"http","versions":["1.1"]},
                        "stream_close_delay":300_000_000_000_u64
                    }),
                    RouteTarget::Redirect { redirect } => json!({
                        "handler":"static_response",
                        "status_code":u16::from(redirect.status),
                        "headers":{"Location":[redirect.location()]}
                    }),
                };
                listener
                    .https
                    .push(json!({"match":[{"host":[host]}],"handle":[handler],"terminal":true}));
                listener.redirects.push(json!({"match":[{"host":[host]}],"handle":[{"handler":"static_response","status_code":308,"headers":{"Location":["https://{http.request.host}{http.request.uri}"]}}],"terminal":true}));
            }
        }
        // The Unix admin endpoint is private to the daemon. Strict SNI matching
        // prevents a TLS connection for one hostname from selecting another host.
        let mut servers = json!({
            "public":public.https_server(&[":443".into()]),
            "public_http":{"listen":[":80"],"routes":public.http_routes()}
        });
        if let Some(proxies) = proxies {
            // Caddy's policies cannot refuse connections without a header, so
            // headers from other peers are ignored, and only tailnet client
            // addresses complete TLS or receive redirects.
            let wrapper =
                json!({"wrapper":"proxy_protocol","allow":proxies,"fallback_policy":"ignore"});
            let tailnet = json!({"ranges":TAILNET_RANGES});
            let mut https = private.https_server(&[format!(":{PRIVATE_HTTPS_PORT}")]);
            https["tls_connection_policies"][0]["match"]["remote_ip"] = tailnet.clone();
            https["listener_wrappers"] = json!([wrapper, {"wrapper":"tls"}]);
            https["automatic_https"] = json!({"disable":true});
            servers["private"] = https;
            let mut redirects = private.http_routes();
            redirects.insert(0, json!({"match":[{"not":[{"remote_ip":tailnet}]}],"handle":[{"handler":"static_response","abort":true}],"terminal":true}));
            servers["private_http"] = json!({
                "listen":[format!(":{PRIVATE_HTTP_PORT}")],"listener_wrappers":[wrapper],
                "routes":redirects,"automatic_https":{"disable":true}
            });
        }
        let mut configuration = json!({
            "admin":{"listen":"unix//control/admin.sock"},
            "apps":{"http":{"servers":servers}}
        });
        let pems = self.certificates.loaded();
        if !pems.is_empty() {
            configuration["apps"]["tls"]["certificates"]["load_pem"] = pems
                .into_iter()
                .map(|(certificate, key)| json!({"certificate":certificate,"key":key}))
                .collect();
        }
        #[cfg(test)]
        if let Some(issuer) = &self.issuer {
            configuration["apps"]["tls"]["automation"] = json!({"policies":[{"issuers":[issuer]}]});
        }
        configuration
    }
}

impl Listener {
    /// An HTTPS server on `listen` that completes TLS only for this listener's
    /// hostnames, even with no hostnames at all.
    fn https_server(&self, listen: &[String]) -> Value {
        let mut routes = self.https.clone();
        routes.push(Self::not_found());
        json!({
            "protocols":["h1","h2"],"listen":listen,"routes":routes,"strict_sni_host":true,
            "tls_connection_policies":[{"match":{"sni":self.hosts}}],
            "automatic_https":{"disable_redirects":true}
        })
    }

    /// This listener's HTTP → HTTPS redirects, then the 404 fallback.
    fn http_routes(&self) -> Vec<Value> {
        let mut routes = self.redirects.clone();
        routes.push(Self::not_found());
        routes
    }

    fn not_found() -> Value {
        json!({"handle":[{"handler":"static_response","status_code":404}],"terminal":true})
    }
}
