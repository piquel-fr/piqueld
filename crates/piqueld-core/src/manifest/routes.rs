//! Exact public hostnames and validated HTTP route destinations.

use crate::{ServiceName, names::validated_string};
use serde::{Deserialize, Serialize};
use std::num::NonZeroU16;
use utoipa::ToSchema;

validated_string!(
    /// Canonical ASCII public DNS hostname; Unicode domains use their IDNA form.
    Hostname, HostnameError,
    "hostname must be an exact public DNS name (use ASCII/Punycode, without a scheme, path, wildcard, or port)",
    |value: &str| value.len() <= 253
        && value.contains('.')
        && value.split('.').all(|label| !label.is_empty() && label.len() <= 63
            && label.as_bytes()[0].is_ascii_alphanumeric()
            && label.as_bytes()[label.len()-1].is_ascii_alphanumeric()
            && label.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
        && value.rsplit('.').next().is_some_and(|tld| tld.bytes().any(|b| b.is_ascii_lowercase()) && !matches!(tld, "localhost" | "local" | "internal"))
);

/// Validated, application-owned HTTP route; TLS terminates at the gateway.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ValidatedRoute {
    /// Exact canonical public hostname.
    pub hostname: Hostname,
    /// Logical HTTP backend service.
    pub service: ServiceName,
    /// Internal HTTP port; never published directly on the host.
    #[schema(value_type = u16, minimum = 1)]
    pub port: NonZeroU16,
}

impl ValidatedRoute {
    pub(super) fn to_input(&self) -> super::input::Route {
        super::input::Route {
            hostname: self.hostname.to_string(),
            service: self.service.to_string(),
            port: self.port.get(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        ApplicationId, InstanceId, ResolutionSet, ResolvedSource, ServiceName, compile_application,
        parse_toml,
    };

    fn manifest(routes: &str) -> String {
        format!(
            "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='notes'\n[[spec.services]]\nname='web'\n[spec.services.source]\ntype='image'\nimage='nginx:alpine'\n{routes}"
        )
    }

    #[test]
    fn exact_hosts_are_canonical_and_invalid_routes_are_rejected() {
        let route = "[[spec.routes]]\nhostname='NOTES.Example.COM.'\nservice='web'\nport=3000";
        let app = parse_toml(&manifest(route))
            .unwrap()
            .normalize(ApplicationId::parse("test-app").unwrap());
        assert_eq!(app.spec().routes[0].hostname.as_str(), "notes.example.com");
        assert_eq!(
            parse_toml(&app.export_toml().unwrap())
                .unwrap()
                .normalize(app.id().clone()),
            app
        );
        for host in [
            "*.example.com",
            "https://example.com",
            "example.com/path",
            "example.com:443",
            "localhost",
            "127.0.0.1",
            "[::1]",
            "a..com",
            "foo.local",
            "é.example.com",
        ] {
            assert!(
                parse_toml(&manifest(&route.replace("NOTES.Example.COM.", host))).is_err(),
                "{host}"
            );
        }
        assert!(parse_toml(&manifest(&route.replace("port=3000", "port=0"))).is_err());
        assert!(
            parse_toml(&manifest(
                &route.replace("service='web'", "service='database'")
            ))
            .is_err()
        );
        assert!(parse_toml(&manifest(&format!("{route}\n{route}"))).is_err());
        let without = parse_toml(&manifest(""))
            .unwrap()
            .normalize(app.id().clone());
        assert_ne!(app.spec_hash(), without.spec_hash());
    }

    #[test]
    fn only_exposed_services_join_ingress_and_disabling_preserves_route_intent() {
        let routes = "[[spec.routes]]\nhostname='notes.example.com'\nservice='web'\nport=3000\n[[spec.services]]\nname='db'\n[spec.services.source]\ntype='image'\nimage='nginx:alpine'";
        let app = parse_toml(&manifest(routes))
            .unwrap()
            .normalize(ApplicationId::parse("test-app").unwrap());
        let sources = ["web", "db"]
            .into_iter()
            .map(|name| {
                (
                    ServiceName::parse(name).unwrap(),
                    ResolvedSource::parse_image(
                        "nginx:alpine",
                        format!("nginx@sha256:{}", "a".repeat(64)),
                    )
                    .unwrap(),
                )
            })
            .collect();
        let target = compile_application(
            &app,
            InstanceId::parse("test-instance").unwrap(),
            &ResolutionSet { sources },
        )
        .unwrap();
        let enabled = target.clone().with_ingress(true);
        assert_eq!(enabled.networks.len(), 2);
        assert!(
            enabled
                .networks
                .iter()
                .all(crate::DesiredNetwork::has_valid_identity)
        );
        assert_eq!(
            enabled
                .services
                .iter()
                .find(|s| s.logical_name.as_str() == "web")
                .unwrap()
                .networks
                .len(),
            2
        );
        assert_eq!(
            enabled
                .services
                .iter()
                .find(|s| s.logical_name.as_str() == "db")
                .unwrap()
                .networks
                .len(),
            1
        );
        assert_eq!(enabled.clone().with_ingress(true), enabled);
        assert_eq!(enabled.with_ingress(false), target);
    }
}
