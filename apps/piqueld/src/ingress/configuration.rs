//! Builds Caddy's runtime JSON from the deployed routing table. The daemon loads
//! it through Caddy's private Unix admin socket and persists it as the gateway's
//! autosave configuration for independent container restarts.
//!
//! Known hosts redirect HTTP to HTTPS, then either proxy to their Swarm service's
//! internal HTTP port or answer with their configured redirect. Caddy
//! obtains/renews certificates for those hosts automatically; explicit
//! redirects keep unknown HTTP hosts on the final 404 handler.

use super::Ingress;
use crate::store::ingress::RoutingTable;
use piqueld_core::{DockerServiceName, manifest::RouteTarget};
use serde_json::{Value, json};

impl Ingress {
    /// Produces the complete replacement configuration. Exact-host routes enable
    /// automatic TLS without on-demand certificate issuance for arbitrary hosts.
    ///
    /// Each route yields a probe handler, a reverse proxy, and an HTTP redirect;
    /// both servers end with a 404 fallback.
    ///
    /// ```text
    /// app.example.com -> reverse_proxy <swarm service name>:<port>
    /// http://app.example.com/x -> 308 https://app.example.com/x
    /// ```
    pub(super) fn configuration(&self, table: &RoutingTable) -> Value {
        let mut https = Vec::new();
        let mut redirects = Vec::new();
        for (id, routes) in table {
            for route in routes {
                let host = route.hostname.as_str();
                // A bounded, side-effect-free endpoint lets the daemon distinguish
                // this gateway from a different server behind an incorrect DNS record.
                https.push(json!({"match":[{"host":[host],"path":["/.well-known/piqueld-ingress"]}],"handle":[{"handler":"static_response","body":self.instance_id}],"terminal":true}));
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
                https.push(json!({"match":[{"host":[host]}],"handle":[handler],"terminal":true}));
                redirects.push(json!({"match":[{"host":[host]}],"handle":[{"handler":"static_response","status_code":308,"headers":{"Location":["https://{http.request.host}{http.request.uri}"]}}],"terminal":true}));
            }
        }
        let not_found =
            json!({"handle":[{"handler":"static_response","status_code":404}],"terminal":true});
        https.push(not_found.clone());
        redirects.push(not_found);
        // The Unix admin endpoint is private to the daemon. Strict SNI matching
        // prevents a TLS connection for one hostname from selecting another host.
        let configuration = json!({
            "admin":{"listen":"unix//control/admin.sock"},
            "apps":{"http":{"servers":{
                "https":{"protocols":["h1","h2"],"listen":[":443"],"routes":https,"strict_sni_host":true,
                    "tls_connection_policies":[{}],"automatic_https":{"disable_redirects":true}},
                "http":{"listen":[":80"],"routes":redirects}
            }}}
        });
        #[cfg(test)]
        if let Some(issuer) = &self.issuer {
            let mut configuration = configuration;
            configuration["apps"]["tls"] =
                json!({"automation":{"policies":[{"issuers":[issuer]}]}});
            return configuration;
        }
        configuration
    }
}
