//! The URLs of an environment's rendered routes and whether each is ready,
//! derived only from state the daemon already observes. Clients read the
//! result; none probes a URL or computes readiness itself.
use crate::api::{DnsRecordState, IngressStatus, ObservedServiceView, PublicIngressStatus};
use crate::manifest::{RouteTarget, ValidatedRoute, Visibility};
use crate::{Convergence, RouteName, ServiceName};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Whether a URL is reachable. A known URL is not a ready one.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UrlState {
    /// At least one condition in `pending` holds.
    Pending,
    /// Served by the gateway over verified HTTPS, with its DNS records in
    /// place and its service healthy.
    Ready,
}

/// `pending` or `ready`.
impl std::fmt::Display for UrlState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
        })
    }
}

/// A condition that keeps a URL pending.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "condition", rename_all = "snake_case")]
pub enum UrlCondition {
    /// The gateway has not acknowledged this exact route yet.
    Ingress,
    /// HTTPS, with the route's certificate, is not verified for this route:
    /// its listener is unhealthy now, or the daemon's latest route check
    /// failed or has not run since the route changed.
    Https {
        /// Why, from the route's status.
        message: String,
    },
    /// The hostname's managed DNS records are not in place.
    Dns {
        /// `pending` or `dns_conflict`.
        state: DnsRecordState,
    },
    /// The route's service is not observed healthy.
    Service {
        /// The route's service.
        service: ServiceName,
    },
}

/// What the condition waits for: `ingress`, `HTTPS (<message>)`,
/// `DNS pending`, or `service web`.
impl std::fmt::Display for UrlCondition {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ingress => formatter.write_str("ingress"),
            Self::Https { message } => write!(formatter, "HTTPS ({message})"),
            Self::Dns { state } => write!(formatter, "DNS {state}"),
            Self::Service { service } => write!(formatter, "service {service}"),
        }
    }
}

/// One URL of a rendered route, and whether it is ready.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct RouteUrl {
    /// The route's name, when it has one.
    #[serde(default)]
    pub name: Option<RouteName>,
    /// `https://` and the rendered hostname.
    pub url: String,
    /// Effective visibility: `private` URLs are reachable only from the tailnet.
    pub visibility: Visibility,
    /// Backend service or redirect.
    #[serde(flatten)]
    pub target: RouteTarget,
    /// `ready` exactly when `pending` is empty.
    pub state: UrlState,
    /// Every condition that keeps the URL pending, in the order above.
    pub pending: Vec<UrlCondition>,
}

impl RouteUrl {
    /// Derives a rendered route's URL and state. It is ready when:
    ///
    /// 1. the gateway acknowledged this exact route (`applied`);
    /// 2. the listener serving its visibility is enabled and healthy now
    ///    (`ingress`), and
    ///    the daemon's latest check of this route (same hostname, visibility,
    ///    and target in `ingress.routes`) found it `ready`: DNS resolves to
    ///    that listener, which serves it with a trusted certificate;
    /// 3. its DNS records are not `pending` or `dns_conflict`;
    /// 4. its service, unless it redirects, is observed healthy.
    #[must_use]
    pub fn derive(
        route: &ValidatedRoute,
        applied: &[ValidatedRoute],
        ingress: &IngressStatus,
        services: &[ObservedServiceView],
    ) -> Self {
        let status = ingress.routes.iter().find(|status| {
            status.hostname == route.hostname.as_str()
                && status.visibility == route.visibility
                && status.target == route.target
        });
        let mut pending = Vec::new();
        if !applied.contains(route) {
            pending.push(UrlCondition::Ingress);
        }
        // A disabled listener never serves the route, checked or not. A
        // listener failure observed since the route's check outdates it;
        // other applications' network failures do not.
        let listener = match (route.visibility, &ingress.public) {
            _ if !ingress.enabled => Some("Ingress is disabled in daemon configuration"),
            (Visibility::Private, _) if !ingress.private.enabled => {
                Some("Private ingress is disabled in daemon configuration")
            }
            (Visibility::Public, _) if !ingress.gateway => Some(ingress.message.as_str()),
            (
                Visibility::Public,
                PublicIngressStatus::Tunnel {
                    connected: false,
                    message,
                    ..
                },
            ) => Some(message.as_str()),
            (Visibility::Public, _) => None,
            (Visibility::Private, _) => {
                (!ingress.private.healthy).then_some(ingress.private.message.as_str())
            }
        };
        let https = match status {
            _ if listener.is_some_and(str::is_empty) => Some("Its listener is unavailable".into()),
            _ if let Some(message) = listener => Some(message.to_owned()),
            Some(status) if status.state == "ready" => None,
            Some(status) => Some(status.message.clone()),
            None => Some(
                "Not checked yet: the daemon verifies HTTPS once the gateway serves the route"
                    .into(),
            ),
        };
        if let Some(message) = https {
            pending.push(UrlCondition::Https { message });
        }
        if let Some(status) = status
            && matches!(
                status.dns_state,
                DnsRecordState::Pending | DnsRecordState::DnsConflict
            )
        {
            pending.push(UrlCondition::Dns {
                state: status.dns_state,
            });
        }
        if let Some(service) = route.target.service()
            && !services
                .iter()
                .any(|observed| observed.name == service.as_str() && observed.healthy())
        {
            pending.push(UrlCondition::Service {
                service: service.clone(),
            });
        }
        Self {
            name: route.name.clone(),
            url: format!("https://{}", route.hostname),
            visibility: route.visibility,
            target: route.target.clone(),
            state: if pending.is_empty() {
                UrlState::Ready
            } else {
                UrlState::Pending
            },
            pending,
        }
    }
}

impl RouteUrl {
    /// What keeps it pending, e.g. `ingress, DNS pending`; empty once ready.
    #[must_use]
    pub fn waiting_for(&self) -> String {
        self.pending
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl ObservedServiceView {
    /// Whether the runtime reports every desired replica running and healthy.
    #[must_use]
    pub fn healthy(&self) -> bool {
        self.convergence == Convergence::Converged && self.healthy_replicas >= self.desired_replicas
    }
}

#[cfg(test)]
mod tests {
    use super::{RouteUrl, UrlCondition, UrlState};
    use crate::api::{
        DnsRecordState, DnsRecords, IngressStatus, ObservedServiceView, PublicIngressStatus,
        RouteStatus,
    };
    use crate::manifest::{RouteTarget, ValidatedRoute, Visibility};
    use crate::{Convergence, RouteName, ServiceName};

    fn route() -> ValidatedRoute {
        ValidatedRoute {
            hostname: crate::manifest::Hostname::parse("notes.example.com").unwrap(),
            name: Some(RouteName::parse("web").unwrap()),
            visibility: Visibility::Public,
            target: RouteTarget::Service {
                service: ServiceName::parse("web").unwrap(),
                port: 3000.try_into().unwrap(),
            },
        }
    }

    fn status(route: &ValidatedRoute) -> RouteStatus {
        RouteStatus {
            environment_id: "app-notes-01".into(),
            name: route.name.clone(),
            hostname: route.hostname.to_string(),
            visibility: route.visibility,
            dns: DnsRecords::ServerAddresses {
                addresses: Vec::new(),
            },
            dns_state: DnsRecordState::Managed,
            target: route.target.clone(),
            state: "ready".into(),
            message: "verified".into(),
        }
    }

    fn service(convergence: Convergence, healthy_replicas: u16) -> ObservedServiceView {
        ObservedServiceView {
            name: "web".into(),
            image: None,
            desired_replicas: 2,
            observed_replicas: 2,
            healthy_replicas,
            convergence,
            diagnostics: Vec::new(),
        }
    }

    /// Derives the URL with one input changed from a ready baseline.
    fn derive(
        change: impl FnOnce(&mut Vec<ValidatedRoute>, &mut IngressStatus, &mut Vec<ObservedServiceView>),
    ) -> RouteUrl {
        let route = route();
        let (mut applied, mut ingress, mut services) = (
            vec![route.clone()],
            ingress(status(&route)),
            vec![service(Convergence::Converged, 2)],
        );
        change(&mut applied, &mut ingress, &mut services);
        RouteUrl::derive(&route, &applied, &ingress, &services)
    }

    /// Healthy listeners with one route status.
    fn ingress(status: RouteStatus) -> IngressStatus {
        let mut ingress = IngressStatus {
            enabled: true,
            healthy: true,
            gateway: true,
            routes: vec![status],
            ..IngressStatus::default()
        };
        ingress.private.healthy = true;
        ingress
    }

    #[test]
    fn a_url_is_ready_only_when_every_condition_holds() {
        let ready = derive(|_, _, _| {});
        assert_eq!(ready.state, UrlState::Ready);
        assert_eq!(ready.url, "https://notes.example.com");
        assert_eq!(ready.name.unwrap().as_str(), "web");
        assert_eq!(ready.pending, []);
    }

    #[test]
    fn each_condition_keeps_a_url_pending() {
        let https = |message: &str| UrlCondition::Https {
            message: message.into(),
        };
        let unchecked =
            https("Not checked yet: the daemon verifies HTTPS once the gateway serves the route");
        let web = UrlCondition::Service {
            service: ServiceName::parse("web").unwrap(),
        };
        let cases: [(&str, RouteUrl, Vec<UrlCondition>); 13] = [
            (
                "gateway failed since the route was checked",
                derive(|_, ingress, _| {
                    ingress.gateway = false;
                    ingress.message = "gateway down".into();
                }),
                vec![https("gateway down")],
            ),
            (
                "tunnel disconnected since the route was checked",
                derive(|_, ingress, _| {
                    ingress.public = PublicIngressStatus::Tunnel {
                        id: "tunnel".into(),
                        connected: false,
                        message: "tunnel disconnected".into(),
                    };
                }),
                vec![https("tunnel disconnected")],
            ),
            (
                "ingress disabled, before the route was checked",
                derive(|_, ingress, _| {
                    ingress.enabled = false;
                    ingress.routes.clear();
                }),
                vec![https("Ingress is disabled in daemon configuration")],
            ),
            (
                "another application's network failed",
                derive(|_, ingress, _| ingress.healthy = false),
                vec![],
            ),
            (
                "not applied by the gateway",
                derive(|applied, _, _| applied.clear()),
                vec![UrlCondition::Ingress],
            ),
            (
                "a different route was applied under the hostname",
                derive(|applied, _, _| applied[0].visibility = Visibility::Private),
                vec![UrlCondition::Ingress],
            ),
            (
                "HTTPS not verified",
                derive(|_, ingress, _| {
                    ingress.routes[0].state = "pending".into();
                    ingress.routes[0].message = "no certificate".into();
                }),
                vec![https("no certificate")],
            ),
            (
                "never checked",
                derive(|_, ingress, _| ingress.routes.clear()),
                vec![unchecked.clone()],
            ),
            (
                "checked before its target changed",
                derive(|_, ingress, _| {
                    ingress.routes[0].target = RouteTarget::Service {
                        service: ServiceName::parse("api").unwrap(),
                        port: 80.try_into().unwrap(),
                    };
                }),
                vec![unchecked],
            ),
            (
                "DNS records pending",
                derive(|_, ingress, _| ingress.routes[0].dns_state = DnsRecordState::Pending),
                vec![UrlCondition::Dns {
                    state: DnsRecordState::Pending,
                }],
            ),
            (
                "DNS conflict",
                derive(|_, ingress, _| ingress.routes[0].dns_state = DnsRecordState::DnsConflict),
                vec![UrlCondition::Dns {
                    state: DnsRecordState::DnsConflict,
                }],
            ),
            (
                "service degraded",
                derive(|_, _, services| services[0] = service(Convergence::Degraded, 1)),
                vec![web.clone()],
            ),
            (
                "service not observed",
                derive(|_, _, services| services.clear()),
                vec![web],
            ),
        ];
        for (case, url, pending) in cases {
            let state = if pending.is_empty() {
                UrlState::Ready
            } else {
                UrlState::Pending
            };
            assert_eq!(url.state, state, "{case}");
            assert_eq!(url.pending, pending, "{case}");
        }
    }

    #[test]
    fn redirects_need_no_service_and_manual_dns_is_not_pending() {
        let mut redirect = route();
        redirect.target = RouteTarget::Redirect {
            redirect: serde_json::from_value(
                serde_json::json!({"to": "https://example.com", "status": 308, "preserve_path": true}),
            )
            .unwrap(),
        };
        let mut status = status(&redirect);
        status.dns_state = DnsRecordState::Manual;
        let url = RouteUrl::derive(
            &redirect,
            std::slice::from_ref(&redirect),
            &ingress(status),
            &[],
        );
        assert_eq!(url.state, UrlState::Ready);
    }
}
