//! Exact public hostnames and validated HTTP route destinations.

use super::{ValidationErrors, input, variables::Template};
use crate::{ServiceName, names::validated_string};
use serde::{Deserialize, Serialize};
use std::{fmt, num::NonZeroU16};
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

impl Hostname {
    /// Whether this is `domain` itself or one of its subdomains.
    ///
    /// ```text
    /// "api.example.com" within "example.com"    -> true
    /// "badexample.com"  within "example.com"    -> false
    /// ```
    #[must_use]
    pub fn is_within(&self, domain: &Self) -> bool {
        self.as_str()
            .strip_suffix(domain.as_str())
            .is_some_and(|prefix| prefix.is_empty() || prefix.ends_with('.'))
    }

    /// The hostname one label up, when that is still a hostname.
    ///
    /// ```text
    /// "api.example.com" -> "example.com"
    /// "example.com"     -> none ("com" has no dot)
    /// ```
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        let (_, parent) = self.as_str().split_once('.')?;
        Self::parse(parent).ok()
    }
}

validated_string!(
    /// Absolute redirect destination: an `http` or `https` scheme, a public
    /// hostname with an optional port, and an optional path. Queries, fragments,
    /// credentials, and Caddy placeholders are rejected.
    RedirectUrl, RedirectUrlError,
    "redirect target must be an http(s) URL with a lowercase public hostname, an optional port and path, and no query, fragment, or braces",
    |value: &str| value.len() <= 2048
        && RedirectUrl::parts(value).is_some_and(|(host, port, path)| {
            Hostname::parse(host).is_ok()
                && port.is_none_or(|port| {
                    !port.starts_with('0')
                        && port.bytes().all(|b| b.is_ascii_digit())
                        && port.parse::<NonZeroU16>().is_ok()
                })
                && path.bytes().all(|b| b.is_ascii_graphic() && !b"?#{}\\".contains(&b))
        })
);

impl RedirectUrl {
    /// Splits a candidate into its host, optional port, and path.
    fn parts(value: &str) -> Option<(&str, Option<&str>, &str)> {
        let rest = value
            .strip_prefix("https://")
            .or_else(|| value.strip_prefix("http://"))?;
        let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        let (host, port) = authority
            .split_once(':')
            .map_or((authority, None), |(host, port)| (host, Some(port)));
        Some((host, port, path))
    }

    /// The destination's hostname.
    #[must_use]
    pub fn hostname(&self) -> &str {
        Self::parts(self.as_str()).map_or("", |(host, ..)| host)
    }
}

/// Invalid visibility input.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("visibility must be public or private")]
pub struct VisibilityError;

/// Who may connect to a route: everyone, or only tailnet devices. It says
/// nothing about how traffic arrives; the installation decides that.
///
/// Variants are ordered from least to most restrictive, so the stricter of
/// two visibilities is their maximum.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Deserialize,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
    ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    /// Reachable from the internet.
    Public,
    /// Reachable only from the tailnet, on the same hostname.
    #[default]
    Private,
}

impl Visibility {
    /// This visibility capped by an environment or preview `ceiling`: the
    /// stricter of the two. A route can only become stricter.
    ///
    /// ```text
    /// public  capped by private -> private
    /// private capped by public  -> private
    /// ```
    #[must_use]
    pub fn capped(self, ceiling: Self) -> Self {
        self.max(ceiling)
    }

    /// Whether this is [`Self::Public`].
    #[must_use]
    pub const fn is_public(&self) -> bool {
        matches!(self, Self::Public)
    }
}

/// `public` or `private`, as written in manifests.
impl fmt::Display for Visibility {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Public => "public",
            Self::Private => "private",
        })
    }
}

impl std::str::FromStr for Visibility {
    type Err = VisibilityError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "public" => Ok(Self::Public),
            "private" => Ok(Self::Private),
            _ => Err(VisibilityError),
        }
    }
}

/// Who may connect to a deployed route and what its backend learns about
/// them. Only private routes can pass the client's tailnet identity, since
/// only the tailnet knows who is connecting.
///
/// It keeps the flat wire shape of the manifest:
///
/// ```text
/// Public                      {"visibility":"public"}
/// Private { identity: false } {"visibility":"private"}
/// Private { identity: true }  {"visibility":"private","identity":true}
/// ```
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "RouteAccessFields", into = "RouteAccessFields")]
pub enum RouteAccess {
    /// Reachable from the internet.
    Public,
    /// Reachable only from the tailnet.
    Private {
        /// Whether the gateway passes the client's tailnet identity to the
        /// backend in `Piqueld-*` request headers.
        identity: bool,
    },
}

/// Who may connect to a route, and whether its backend receives the client's
/// tailnet identity: the flat wire fields of a [`RouteAccess`].
#[derive(Clone, Copy, Deserialize, Serialize, ToSchema)]
struct RouteAccessFields {
    /// Who may connect: everyone, or only tailnet devices.
    #[serde(default)]
    visibility: Visibility,
    /// Whether the backend receives the client's tailnet identity; private
    /// routes only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    identity: bool,
}

/// `identity` was requested on a public route.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("identity requires visibility = \"private\" on the route itself")]
pub struct PublicIdentityError;

impl RouteAccess {
    /// Combines a visibility with whether the route asks for identity.
    ///
    /// # Errors
    /// Public routes cannot ask for identity.
    pub const fn new(visibility: Visibility, identity: bool) -> Result<Self, PublicIdentityError> {
        match (visibility, identity) {
            (Visibility::Public, true) => Err(PublicIdentityError),
            (Visibility::Public, false) => Ok(Self::Public),
            (Visibility::Private, identity) => Ok(Self::Private { identity }),
        }
    }

    /// Who may connect, which selects the listener serving the route.
    #[must_use]
    pub const fn visibility(self) -> Visibility {
        match self {
            Self::Public => Visibility::Public,
            Self::Private { .. } => Visibility::Private,
        }
    }

    /// Whether the backend receives the client's tailnet identity.
    #[must_use]
    pub const fn identity(self) -> bool {
        matches!(self, Self::Private { identity: true })
    }
}

/// A route with `visibility` and no identity.
impl From<Visibility> for RouteAccess {
    fn from(visibility: Visibility) -> Self {
        match visibility {
            Visibility::Public => Self::Public,
            Visibility::Private => Self::Private { identity: false },
        }
    }
}

impl TryFrom<RouteAccessFields> for RouteAccess {
    type Error = PublicIdentityError;

    fn try_from(fields: RouteAccessFields) -> Result<Self, Self::Error> {
        Self::new(fields.visibility, fields.identity)
    }
}

impl From<RouteAccess> for RouteAccessFields {
    fn from(access: RouteAccess) -> Self {
        Self {
            visibility: access.visibility(),
            identity: access.identity(),
        }
    }
}

impl utoipa::PartialSchema for RouteAccess {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        RouteAccessFields::schema()
    }
}

impl ToSchema for RouteAccess {
    fn name() -> std::borrow::Cow<'static, str> {
        "RouteAccess".into()
    }
}

/// Invalid redirect status input.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("redirect status must be 301, 302, 303, 307, or 308")]
pub struct RedirectStatusError;

/// HTTP redirect status code, serialized as its number.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "u16", into = "u16")]
pub enum RedirectStatus {
    /// 301: permanent; clients may change the method to GET.
    MovedPermanently = 301,
    /// 302: temporary; clients may change the method to GET.
    Found = 302,
    /// 303: temporary; clients always follow with GET.
    SeeOther = 303,
    /// 307: temporary; method and body are preserved.
    TemporaryRedirect = 307,
    /// 308: permanent; method and body are preserved.
    PermanentRedirect = 308,
}

impl From<RedirectStatus> for u16 {
    fn from(status: RedirectStatus) -> Self {
        status as Self
    }
}

impl TryFrom<u16> for RedirectStatus {
    type Error = RedirectStatusError;

    fn try_from(code: u16) -> Result<Self, Self::Error> {
        Ok(match code {
            301 => Self::MovedPermanently,
            302 => Self::Found,
            303 => Self::SeeOther,
            307 => Self::TemporaryRedirect,
            308 => Self::PermanentRedirect,
            _ => return Err(RedirectStatusError),
        })
    }
}

/// An HTTP redirect answered by the gateway itself.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ValidatedRedirect {
    /// Absolute destination URL.
    pub to: RedirectUrl,
    /// HTTP redirect status code.
    #[schema(value_type = u16, minimum = 301, maximum = 308)]
    pub status: RedirectStatus,
    /// Appends the request path and query to `to`.
    pub preserve_path: bool,
}

impl ValidatedRedirect {
    /// The `Location` header value, using Caddy placeholders for the request URI.
    #[must_use]
    pub fn location(&self) -> String {
        if self.preserve_path {
            let base = self.to.as_str().trim_end_matches('/');
            format!("{base}{{http.request.uri}}")
        } else {
            self.to.to_string()
        }
    }
}

/// What a route's hostname serves. The variants keep their historical flat
/// wire shape (`service`/`port`, or `redirect`) inside [`ValidatedRoute`].
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(untagged, try_from = "RouteTargetFields")]
pub enum RouteTarget {
    /// Proxies to a service in the same application.
    Service {
        /// Logical HTTP backend service.
        service: ServiceName,
        /// Internal HTTP port; never published directly on the host.
        #[schema(value_type = u16, minimum = 1)]
        port: NonZeroU16,
    },
    /// Redirects every request without reaching a backend.
    Redirect {
        /// Redirect destination and behavior.
        redirect: ValidatedRedirect,
    },
}

/// Flat wire fields of a [`RouteTarget`]. Decoding through them rejects routes
/// that mix or omit destinations, which an untagged enum would silently accept.
#[derive(Deserialize)]
struct RouteTargetFields {
    service: Option<ServiceName>,
    port: Option<NonZeroU16>,
    redirect: Option<ValidatedRedirect>,
}

impl TryFrom<RouteTargetFields> for RouteTarget {
    type Error = &'static str;

    fn try_from(fields: RouteTargetFields) -> Result<Self, Self::Error> {
        match (fields.service, fields.port, fields.redirect) {
            (Some(service), Some(port), None) => Ok(Self::Service { service, port }),
            (None, None, Some(redirect)) => Ok(Self::Redirect { redirect }),
            _ => Err(super::validation::ROUTE_TARGET_MESSAGE),
        }
    }
}

impl RouteTarget {
    /// The backend service, which redirects do not have.
    #[must_use]
    pub fn service(&self) -> Option<&ServiceName> {
        match self {
            Self::Service { service, .. } => Some(service),
            Self::Redirect { .. } => None,
        }
    }
}

impl fmt::Display for RouteTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Service { service, port } => write!(formatter, "{service}:{port}"),
            Self::Redirect { redirect } => {
                write!(formatter, "{} → ", u16::from(redirect.status))?;
                if redirect.preserve_path {
                    write!(
                        formatter,
                        "{}/*",
                        redirect.to.as_str().trim_end_matches('/')
                    )
                } else {
                    write!(formatter, "{}", redirect.to)
                }
            }
        }
    }
}

/// Validated, application-owned HTTP route; TLS terminates at the gateway.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
pub struct ValidatedRoute {
    /// Exact canonical public hostname.
    pub hostname: Hostname,
    /// Effective visibility, the route's own capped by its environment's
    /// ceiling when the manifest was rendered, and whether it passes identity.
    #[serde(flatten)]
    pub access: RouteAccess,
    /// Backend service or redirect.
    #[serde(flatten)]
    pub target: RouteTarget,
}

impl ValidatedRoute {
    /// Whether `other` serves the same hostname on the same listener. A route
    /// that no longer matches any new route is withdrawn, so changing its
    /// visibility withdraws it like a removal.
    #[must_use]
    pub fn same_listener(&self, other: &Self) -> bool {
        self.hostname == other.hostname && self.access.visibility() == other.access.visibility()
    }

    /// Converts input already checked by manifest validation.
    pub(super) fn from_input(route: input::Route, index: usize) -> Result<Self, ValidationErrors> {
        let path = format!("spec.routes[{index}]");
        let invalid = |error: &dyn fmt::Display| ValidationErrors::invalid_name(&path, error);
        let literal = |template: Template| {
            template
                .as_literal()
                .ok_or_else(|| invalid(&"references must be rendered for an environment first"))
        };
        let fields = RouteTargetFields {
            service: route
                .service
                .map(ServiceName::parse)
                .transpose()
                .map_err(|e| invalid(&e))?,
            port: route
                .port
                .map(|port| NonZeroU16::new(port).ok_or_else(|| invalid(&"port must be nonzero")))
                .transpose()?,
            redirect: route
                .redirect
                .map(|redirect| {
                    Ok::<_, ValidationErrors>(ValidatedRedirect {
                        to: RedirectUrl::parse(literal(redirect.to)?).map_err(|e| invalid(&e))?,
                        status: redirect.status.try_into().map_err(|e| invalid(&e))?,
                        preserve_path: redirect.preserve_path,
                    })
                })
                .transpose()?,
        };
        Ok(Self {
            hostname: Hostname::parse(literal(route.hostname)?).map_err(|e| invalid(&e))?,
            access: RouteAccess::new(route.visibility, route.identity).map_err(|e| invalid(&e))?,
            target: fields.try_into().map_err(|e| invalid(&e))?,
        })
    }

    /// Converts back to the editable input shape used for export.
    pub(super) fn to_input(&self) -> input::Route {
        let (service, port, redirect) = match &self.target {
            RouteTarget::Service { service, port } => {
                (Some(service.to_string()), Some(port.get()), None)
            }
            RouteTarget::Redirect { redirect } => (
                None,
                None,
                Some(input::Redirect {
                    to: Template::literal(redirect.to.as_str()),
                    status: redirect.status.into(),
                    preserve_path: redirect.preserve_path,
                }),
            ),
        };
        input::Route {
            hostname: Template::literal(self.hostname.as_str()),
            visibility: self.access.visibility(),
            identity: self.access.identity(),
            service,
            port,
            redirect,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RouteTarget, ValidatedRoute};
    use crate::{
        ApplicationId, EnvironmentId, InstanceId, ResolutionSet, ResolvedSource, ServiceName,
        api::RouteStatus, compile_application, parse_toml,
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
    fn redirects_are_validated_and_keep_the_flat_wire_shape() {
        let redirect = |fields: &str| {
            parse_toml(&manifest(&format!(
                "[[spec.routes]]\nhostname='www.example.com'\n{fields}"
            )))
        };
        let app = redirect("redirect={to='https://example.com/'}")
            .unwrap()
            .normalize(ApplicationId::parse("test-app").unwrap());
        let route = &app.spec().routes[0];
        assert_eq!(
            serde_json::to_value(route).unwrap(),
            serde_json::json!({"hostname":"www.example.com","visibility":"private","redirect":{"to":"https://example.com/","status":308,"preserve_path":true}})
        );
        let RouteTarget::Redirect { redirect: target } = &route.target else {
            panic!("redirect route")
        };
        assert_eq!(target.location(), "https://example.com{http.request.uri}");
        assert_eq!(
            parse_toml(&app.export_toml().unwrap())
                .unwrap()
                .normalize(app.id().clone()),
            app
        );
        let fixed =
            redirect("redirect={to='http://example.com:8080/a',status=302,preserve_path=false}")
                .unwrap()
                .normalize(app.id().clone());
        let RouteTarget::Redirect { redirect: fixed } = &fixed.spec().routes[0].target else {
            panic!("redirect route")
        };
        assert_eq!(fixed.location(), "http://example.com:8080/a");

        for (fields, code) in [
            (
                "service='web'\nport=80\nredirect={to='https://example.com'}",
                "route_target_invalid",
            ),
            ("", "route_target_invalid"),
            ("service='web'", "route_target_invalid"),
            (
                "redirect={to='https://example.com',status=200}",
                "route_redirect_status_invalid",
            ),
            (
                "redirect={to='https://www.example.com/'}",
                "route_redirect_loop",
            ),
            (
                "redirect={to='ftp://example.com'}",
                "route_redirect_invalid",
            ),
            (
                "redirect={to='https://example.com/?q=1'}",
                "route_redirect_invalid",
            ),
            (
                "redirect={to='https://example.com/{http.request.host}'}",
                "route_redirect_invalid",
            ),
            (
                "redirect={to='https://user@example.com'}",
                "route_redirect_invalid",
            ),
            (
                "redirect={to='https://example.com:0'}",
                "route_redirect_invalid",
            ),
            (
                "redirect={to='https://Example.com'}",
                "route_redirect_invalid",
            ),
        ] {
            let errors = redirect(fields).unwrap_err();
            assert!(
                errors.0.iter().any(|e| e.code == code),
                "{fields}: {errors:?}"
            );
        }
    }

    #[test]
    fn decoding_keeps_stored_routes_and_rejects_mixed_targets() {
        // Stored proxy routes predate redirects and must still decode.
        let stored: ValidatedRoute = serde_json::from_value(
            serde_json::json!({"hostname":"a.example.com","service":"web","port":80}),
        )
        .unwrap();
        assert_eq!(stored.target.to_string(), "web:80");
        assert_eq!(
            stored.access,
            super::RouteAccess::Private { identity: false }
        );
        // Identity keeps the flat wire shape and cannot be decoded on a public route.
        let identity = serde_json::json!({"hostname":"a.example.com","visibility":"private","identity":true,"service":"web","port":80});
        let route: ValidatedRoute = serde_json::from_value(identity.clone()).unwrap();
        assert_eq!(route.access, super::RouteAccess::Private { identity: true });
        assert_eq!(serde_json::to_value(&route).unwrap(), identity);
        let mut public = identity;
        public["visibility"] = "public".into();
        assert!(serde_json::from_value::<ValidatedRoute>(public).is_err());
        let redirect_json =
            serde_json::json!({"to":"https://example.com/","status":308,"preserve_path":true});
        for target in [
            serde_json::json!({"service":"web","port":80,"redirect":redirect_json}),
            serde_json::json!({"service":"web"}),
            serde_json::json!({}),
        ] {
            let mut route = serde_json::json!({"hostname":"a.example.com"});
            route
                .as_object_mut()
                .unwrap()
                .extend(target.as_object().unwrap().clone());
            assert!(
                serde_json::from_value::<ValidatedRoute>(route.clone()).is_err(),
                "{route}"
            );
            route["environment_id"] = "test-app".into();
            route["visibility"] = "private".into();
            route["dns"] = serde_json::json!({"type":"server_addresses"});
            route["state"] = "ready".into();
            route["message"] = "".into();
            assert!(
                serde_json::from_value::<RouteStatus>(route.clone()).is_err(),
                "{route}"
            );
        }
    }

    #[test]
    fn redirect_only_applications_have_no_ingress_network() {
        let app = parse_toml("api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='old'\n[[spec.routes]]\nhostname='old.example.com'\nredirect={to='https://new.example.com'}")
            .unwrap()
            .normalize(ApplicationId::parse("test-app").unwrap());
        let target = compile_application(
            &app,
            &EnvironmentId::parse("test-app").unwrap(),
            InstanceId::parse("test-instance").unwrap(),
            &ResolutionSet::default(),
        )
        .unwrap()
        .with_ingress(true);
        assert_eq!(target.networks, []);
        assert_eq!(target.routes, app.spec().routes);
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
            &EnvironmentId::parse("test-app").unwrap(),
            InstanceId::parse("test-instance").unwrap(),
            &ResolutionSet {
                sources,
                ..ResolutionSet::default()
            },
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
