//! Sidebar navigation, overview metrics, application directory, and recent deployments.
use super::format::{bytes, timestamp};
use super::ui::{
    Icon, PageHeader, Tone, badge, empty, health_badge, icon, metric, notice, operation_badge, when,
};
use super::{ApplicationRow, ClientErrorExt, access::can, dashboard_context, management};
use crate::state::{ApplicationHealth, ConnectionState, DataState};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use piqueld_client::{
    Client, PreviewUsage,
    access::{GlobalPermission, Permission},
    system::{
        CertificateStatus, DependencyStatus, DnsProviderStatus, ImageStatus, PublicIngressStatus,
        ReadinessStatus, TelemetryStatus,
    },
};
use std::fmt::Write as _;

impl ConnectionState {
    /// Label of the connection state.
    const fn label(self) -> &'static str {
        match self {
            Self::Loading => "Checking…",
            Self::Reachable => "Reachable",
            Self::Failed => "Request failed",
            Self::Unreachable => "Unreachable",
        }
    }

    /// Tone of the connection dot and card.
    const fn tone(self) -> Tone {
        match self {
            Self::Loading => Tone::Pending,
            Self::Reachable => Tone::Ok,
            Self::Failed | Self::Unreachable => Tone::Bad,
        }
    }
}

/// Navigation, the daemon connection, and the signed-in principal with a
/// sign-out button.
#[component]
pub(super) fn Sidebar() -> impl IntoView {
    let signals = dashboard_context().signals;
    let who = super::auth::signed_in();
    // Pages the caller could not load anything on are hidden; the daemon
    // enforces every permission regardless.
    let system = can(Permission::Global(GlobalPermission::SystemRead));
    let display_name = move || who.get().map(|who| who.name().to_owned());
    let initial = move || {
        who.get()
            .and_then(|who| who.name().chars().next())
            .map_or_else(|| "?".to_owned(), |first| first.to_string())
    };
    view! {
        <aside class="sidebar">
            <A attr:class="brand" href="/dashboard/">
                <span class="brand-mark" aria-hidden="true">
                    "p"
                </span>
                "piqueld"
            </A>
            <nav class="nav" aria-label="Dashboard navigation">
                <div class="nav-group">
                    {nav_link("/dashboard/", Icon::Home, "Overview", true)}
                    {nav_link("/dashboard/applications", Icon::Apps, "Applications", false)}
                    {nav_link("/dashboard/builds", Icon::Builds, "Builds", false)}
                </div>
                <div class="nav-group">
                    <span class="nav-group-label">"Observe"</span>
                    {nav_link("/dashboard/events", Icon::Events, "Events", false)}
                    {nav_link("/dashboard/errors", Icon::Errors, "Errors", false)}
                    {nav_link("/dashboard/analytics", Icon::Analytics, "Analytics", false)}
                    <Show when={move || {
                        system.get()
                    }}>
                        {nav_link(
                            "/dashboard/notifications",
                            Icon::Notifications,
                            "Notifications",
                            false,
                        )}
                    </Show>
                </div>
                <div class="nav-group">
                    <span class="nav-group-label">"System"</span>
                    <Show when={move || {
                        system.get()
                    }}>
                        {nav_link("/dashboard/system", Icon::Daemon, "Daemon status", false)}
                        {nav_link("/dashboard/settings", Icon::Settings, "Host settings", false)}
                    </Show>
                    {nav_link("/dashboard/accounts", Icon::Accounts, "Accounts", false)}
                    {nav_link("/dashboard/audit", Icon::Events, "Audit", false)}
                </div>
            </nav>
            <div class="sidebar-footer">
                <span class="connection">
                    <span
                        class="dot"
                        data-tone={move || signals.connection.get().tone().attr()}
                    ></span>
                    {move || signals.connection.get().label()}
                </span>
                <div class="user-row">
                    <span class="avatar" aria-hidden="true">
                        {initial}
                    </span>
                    <span class="user-name" title={display_name}>
                        {display_name}
                    </span>
                    <super::auth::Logout compact=true />
                </div>
            </div>
        </aside>
    }
}

/// Sidebar link to `href`; `exact` links are only current on that very page.
fn nav_link(href: &'static str, glyph: Icon, label: &'static str, exact: bool) -> AnyView {
    view! {
        <A href={href} attr:class="nav-link" exact={exact}>
            {icon(glyph)}
            {label}
        </A>
    }
    .into_any()
}

/// Overview page: application counts by health, system readiness and the
/// newest deployments, all read from the shared dashboard signals.
#[component]
pub(super) fn OverviewPage() -> impl IntoView {
    let signals = dashboard_context().signals;
    let count = move |predicate: fn(ApplicationHealth) -> bool| {
        signals
            .applications
            .get()
            .iter()
            .filter(|row| predicate(row.health()))
            .count()
    };
    view! {
        <PageHeader
            title="Overview"
            description="Application health, deployment readiness, and the latest activity on this host."
        >
            <management::CreateApplication />
        </PageHeader>
        <div class="stack">
            <div class="metrics">
                <A href="/dashboard/applications" attr:class="metric">
                    <span>"Applications"</span>
                    <strong>{move || signals.applications.get().len()}</strong>
                </A>
                {metric(
                    "Healthy",
                    move || count(|health| health == ApplicationHealth::Converged),
                    None::<&str>,
                )}
                {metric(
                    "Needs attention",
                    move || {
                        count(|health| {
                            matches!(
                                health,
                                ApplicationHealth::Failed | ApplicationHealth::Degraded
                            )
                        })
                    },
                    None::<&str>,
                )}
                {metric(
                    "Daemon",
                    move || {
                        signals
                            .system
                            .get()
                            .map_or_else(|| "—".into(), |system| system.daemon_version)
                    },
                    Some(move || {
                        signals.system.get().map(|system| format!("API {}", system.api_version))
                    }),
                )}
            </div>
            <ReadinessPanel />
            <RecentDeployments />
        </div>
    }
}

/// Daemon connectivity and dependency readiness cards (database, Docker, Swarm,
/// ingress), with a button that triggers a manual dashboard refresh and, when
/// DNS providers are configured and the caller holds `system:operate`, one
/// that checks their credentials now.
#[component]
#[expect(clippy::too_many_lines, reason = "flat view! markup")]
pub(super) fn ReadinessPanel() -> impl IntoView {
    let context = dashboard_context();
    let signals = context.signals;
    let refresh = context.refresh;
    let operate = can(Permission::Global(GlobalPermission::SystemOperate));
    let checking_dns = RwSignal::new(false);
    let dns_error = RwSignal::new(None::<String>);
    let check_dns = move |_| {
        checking_dns.set(true);
        spawn_local(async move {
            match Client::browser().refresh_dns().await {
                Ok(dns) => {
                    dns_error.set(None);
                    signals.system.update(|system| {
                        if let Some(system) = system {
                            system.dns = dns;
                        }
                    });
                }
                Err(error) => dns_error.set(Some(error.user_message())),
            }
            checking_dns.set(false);
        });
    };

    view! {
        <section class="card" aria-labelledby="system-readiness-heading">
            <header>
                <div>
                    <h3 id="system-readiness-heading">"System status"</h3>
                    <p>"Daemon connectivity and the services required to deploy applications."</p>
                </div>
                <div class="btn-group">
                    {move || {
                        let providers = signals
                            .system
                            .with(|system| {
                                system
                                    .as_ref()
                                    .is_some_and(|system| !system.dns.providers.is_empty())
                            });
                        (providers && operate.get())
                            .then(|| {
                                view! {
                                    <button
                                        type="button"
                                        class="btn btn-sm"
                                        disabled={move || checking_dns.get()}
                                        on:click={check_dns}
                                    >
                                        {icon(Icon::Key)}
                                        {move || {
                                            if checking_dns.get() {
                                                "Checking…"
                                            } else {
                                                "Check DNS providers"
                                            }
                                        }}
                                    </button>
                                }
                            })
                    }}
                    <button
                        type="button"
                        class="btn btn-sm"
                        disabled={move || signals.refreshing.get()}
                        on:click={move |_| refresh.run(())}
                    >
                        {icon(Icon::Refresh)}
                        {move || if signals.refreshing.get() { "Refreshing…" } else { "Refresh" }}
                    </button>
                </div>
            </header>
            <div class="stack-sm" aria-live="polite">
                {move || {
                    signals
                        .readiness_error
                        .get()
                        .map(|error| notice(Tone::Bad, format!("Readiness check failed: {error}")))
                }}
                {move || {
                    dns_error
                        .get()
                        .map(|error| notice(
                            Tone::Bad,
                            format!("DNS provider check failed: {error}"),
                        ))
                }}
                <div class="status-grid">
                    {move || connection_card(signals.connection.get())}
                    {move || {
                        signals
                            .system
                            .get()
                            .filter(|system| system.tailscale.enabled)
                            .map(|system| {
                                health_card(
                                    "Tailnet node",
                                    system.tailscale.healthy,
                                    &system.tailscale.message,
                                )
                            })
                    }}
                    {move || {
                        signals.system.get().map(|system| preview_card(system.previews.as_ref()))
                    }} {move || signals.system.get().map(|system| images_card(&system.images))}
                    {move || signals.system.get().map(|system| telemetry_cards(&system.telemetry))}
                    {move || {
                        signals
                            .system
                            .get()
                            .map(|system| {
                                system
                                    .dns
                                    .providers
                                    .iter()
                                    .map(dns_provider_card)
                                    .chain(system.dns.certificates.iter().map(certificate_card))
                                    .collect_view()
                            })
                    }} {move || signals.readiness.get().map(|status| readiness_cards(&status))}
                </div>
            </div>
        </section>
    }
}

/// Cards for the daemon's dependencies and its ingress: the HTTPS gateway,
/// its Cloudflare Tunnel in tunnel mode, and its private listener.
fn readiness_cards(status: &ReadinessStatus) -> AnyView {
    let ingress = &status.ingress;
    let (tone, label) = if !ingress.healthy {
        (Tone::Bad, "Unhealthy")
    } else if !ingress.enabled {
        (Tone::Neutral, "Disabled")
    } else {
        (Tone::Ok, "Ready")
    };
    // In tunnel mode, public routes' CNAME records point at the tunnel ID.
    let tunnel_card = match &ingress.public {
        PublicIngressStatus::Tunnel {
            id,
            connected,
            message,
        } if ingress.enabled => {
            let (tone, label) = if *connected {
                (Tone::Ok, "Connected")
            } else {
                (Tone::Bad, "Disconnected")
            };
            Some(status_card(
                "Cloudflare Tunnel",
                tone,
                label,
                &format!("Tunnel {id}. {message}"),
            ))
        }
        _ => None,
    };
    // The apps node's name and addresses are what private routes' DNS
    // records point at.
    let private = &ingress.private;
    let private_card = (ingress.enabled && private.enabled).then(|| {
        let node = match (&private.dns_name, private.addresses.is_empty()) {
            (Some(name), false) => format!("{name} ({}). ", private.addresses.join(", ")),
            _ => String::new(),
        };
        health_card(
            "Private listener",
            private.healthy,
            &format!("{node}{}", private.message),
        )
    });
    view! {
        {dependency_card("Database", &status.database)}
        {dependency_card("Docker Engine", &status.docker)}
        {dependency_card("Swarm manager", &status.swarm)}
        {status_card("HTTPS ingress", tone, label, &ingress.message)}
        {tunnel_card}
        {private_card}
    }
    .into_any()
}

/// Card for one DNS provider: its kind, discovered zones, health and whether
/// it manages route records.
fn dns_provider_card(provider: &DnsProviderStatus) -> AnyView {
    let zones = if provider.zones.is_empty() {
        "no zones".to_owned()
    } else {
        provider.zones.join(", ")
    };
    health_card(
        "DNS provider",
        provider.healthy,
        &format!(
            "{}: {zones}. {}. {}",
            provider.kind,
            provider.message,
            if provider.manage_records {
                "Manages route DNS records"
            } else {
                "Route DNS records are manual"
            }
        ),
    )
}

/// Card for previews against their `[previews]` limits: the installation's
/// count, each readable application's, and the limits of their replicas. A
/// count at its limit refuses new previews; one over it, after the limit was
/// lowered, keeps its existing previews running.
fn preview_card(usage: Option<&PreviewUsage>) -> AnyView {
    let Some(usage) = usage else {
        return status_card(
            "Previews",
            Tone::Neutral,
            "Unavailable",
            "The daemon could not count previews.",
        );
    };
    let limits = &usage.limits;
    let counts = std::iter::once((usage.total, limits.max_total))
        .chain(
            usage
                .applications
                .iter()
                .map(|application| (application.previews, limits.max_per_application)),
        )
        .collect::<Vec<_>>();
    let (tone, label) = if counts.iter().any(|(count, max)| count > max) {
        (Tone::Bad, "Over limit")
    } else if counts.iter().any(|(count, max)| count == max) {
        (Tone::Warn, "At limit")
    } else {
        (Tone::Ok, "Within limits")
    };
    let applications = usage
        .applications
        .iter()
        .fold(String::new(), |mut text, application| {
            let _ = write!(
                text,
                " {}: {} of {}.",
                application.name, application.previews, limits.max_per_application
            );
            text
        });
    let unlimited = if usage.unlimited_replicas > 0 {
        format!(
            " {} replicas deployed before the limits applied run without them until redeployed.",
            usage.unlimited_replicas
        )
    } else {
        String::new()
    };
    status_card(
        "Previews",
        tone,
        label,
        &format!(
            "{} of {} previews.{applications} Their replicas are limited to {} millicores and {} in total.{unlimited} Services without limits get {} millicores and {}, and replicas are capped at {}.",
            usage.total,
            limits.max_total,
            usage.cpu_millis,
            bytes(usage.memory_bytes),
            limits.default_cpu_millis,
            bytes(limits.default_memory_bytes),
            limits.max_replicas,
        ),
    )
}

/// Card for the images this installation built: how many cleanup kept, and
/// what it removed since the daemon started.
fn images_card(images: &ImageStatus) -> AnyView {
    match images.cleaned_at_ms {
        Some(at) => status_card(
            "Built images",
            Tone::Ok,
            "Cleaned up",
            &format!(
                "{} kept. Cleanup removed {} of images since the daemon started, last {}.",
                images.images,
                bytes(images.reclaimed_bytes),
                timestamp(at)
            ),
        ),
        None => status_card(
            "Built images",
            Tone::Pending,
            "Not cleaned up",
            "Cleanup hasn't run since the daemon started.",
        ),
    }
}

/// Cards for telemetry export: one per collector, with its endpoint, last
/// export and dropped records, or one saying export is off. Headers appear
/// only as the file they were read from.
fn telemetry_cards(telemetry: &TelemetryStatus) -> AnyView {
    if telemetry.destinations.is_empty() {
        return status_card(
            "Telemetry export",
            Tone::Neutral,
            "Off",
            "Set an endpoint under [telemetry] in the daemon configuration to export traces, metrics and logs over OTLP.",
        );
    }
    telemetry
        .destinations
        .iter()
        .map(|destination| {
            let (tone, label) = match (destination.failing, destination.last_success_ms) {
                (true, _) => (Tone::Bad, "Failing"),
                (false, Some(_)) => (Tone::Ok, "Exporting"),
                (false, None) => (Tone::Pending, "Waiting"),
            };
            let last = destination.last_success_ms.map_or_else(
                || "Nothing exported yet.".to_owned(),
                |at| format!("Last export {}.", super::format::relative(at)),
            );
            let headers = destination
                .headers
                .as_ref()
                .map(|headers| format!(" Headers {headers}."))
                .unwrap_or_default();
            status_card(
                "Telemetry export",
                tone,
                label,
                &format!(
                    "{}. {last} {} records dropped since the daemon started.{headers} Exports {}.",
                    destination.endpoint,
                    destination.dropped,
                    telemetry.exported()
                ),
            )
        })
        .collect_view()
        .into_any()
}

/// Card for one DNS-01 certificate: its name, hostnames, expiry and last error.
fn certificate_card(certificate: &CertificateStatus) -> AnyView {
    let (tone, label) = match (&certificate.error, certificate.expires_at_ms) {
        (Some(_), None) => (Tone::Bad, "Failed"),
        (Some(_), Some(_)) => (Tone::Warn, "Renewal failing"),
        (None, Some(_)) => (Tone::Ok, "Issued"),
        (None, None) => (Tone::Pending, "Pending"),
    };
    let hostnames = if certificate.hostnames.is_empty() {
        "no routes".to_owned()
    } else {
        certificate.hostnames.join(", ")
    };
    let expiry = certificate.expires_at_ms.map_or_else(
        || "not issued yet".to_owned(),
        |at| format!("expires {}", timestamp(at)),
    );
    let error = certificate
        .error
        .as_ref()
        .map(|error| format!(" Last error: {error}"))
        .unwrap_or_default();
    status_card(
        "Certificate",
        tone,
        label,
        &format!("{} for {hostnames}, {expiry}.{error}", certificate.name),
    )
}

/// Readiness card for the browser's connection to the daemon itself.
fn connection_card(state: ConnectionState) -> AnyView {
    let message = match state {
        ConnectionState::Loading => "Waiting for the daemon",
        ConnectionState::Reachable => "The API is responding",
        ConnectionState::Failed => "The daemon returned an error",
        ConnectionState::Unreachable => "The dashboard cannot connect to piqueld",
    };
    status_card("piqueld daemon", state.tone(), state.label(), message)
}

/// Readiness card for one daemon dependency.
fn dependency_card(name: &'static str, status: &DependencyStatus) -> AnyView {
    match status {
        DependencyStatus::Ready => status_card(name, Tone::Ok, "Ready", "Available"),
        DependencyStatus::Failed { message } => status_card(name, Tone::Bad, "Failed", message),
    }
}

/// Card for a component that is either ready or unhealthy.
fn health_card(name: &'static str, healthy: bool, message: &str) -> AnyView {
    if healthy {
        status_card(name, Tone::Ok, "Ready", message)
    } else {
        status_card(name, Tone::Bad, "Unhealthy", message)
    }
}

/// Card with a `tone`d status `label` and a `message` explaining it.
fn status_card(name: &'static str, tone: Tone, label: &'static str, message: &str) -> AnyView {
    view! {
        <article class="status-card" data-tone={tone.attr()}>
            <header>
                <strong>{name}</strong>
                {badge(tone, label)}
            </header>
            <p>{message.to_owned()}</p>
        </article>
    }
    .into_any()
}

/// Application directory linking to each application's detail page, with
/// loading/empty states and recent deployments.
#[component]
pub(super) fn ApplicationsPage() -> impl IntoView {
    let signals = dashboard_context().signals;
    view! {
        <PageHeader
            title="Applications"
            description="Saved configuration, runtime health, and deployment history for every application."
        >
            <management::CreateApplication />
        </PageHeader>
        <div class="stack">
            <section class="list" aria-label="Applications">
                {move || {
                    signals
                        .applications
                        .get()
                        .into_iter()
                        .map(ApplicationRow::list_row)
                        .collect_view()
                }}
                {move || match signals.data_state.get() {
                    DataState::Loading => empty("Loading applications…"),
                    DataState::Empty => empty("No applications yet. Create one to get started."),
                    _ => ().into_any(),
                }}
            </section>
            <RecentDeployments />
        </div>
    }
}

impl ApplicationRow {
    /// Directory row linking to the application's page, with its latest
    /// deployment and health.
    fn list_row(self) -> AnyView {
        let health = self.health();
        let subtitle = if self.application.delete_intent {
            "Deletion requested".to_owned()
        } else {
            self.message()
                .unwrap_or_else(|| format!("Generation {}", self.application.generation))
        };
        let latest = self
            .deployments
            .iter()
            .max_by_key(|deployment| deployment.operation.created_at_ms)
            .map(|deployment| deployment.operation.created_at_ms);
        view! {
            <A
                attr:class="list-row"
                href={format!("/dashboard/applications/{}", self.application.id)}
            >
                <span class="app-icon" aria-hidden="true">
                    {icon(Icon::Package)}
                </span>
                <span class="title">{self.application.name.clone()} <small>{subtitle}</small></span>
                <span class="meta">
                    {latest
                        .map_or_else(
                            || "Never deployed".into_any(),
                            |ms| {
                                view! {
                                    "Deployed "
                                    {when(ms)}
                                }
                                    .into_any()
                            },
                        )}
                </span>
                {health_badge(health)}
                <span class="chevron" aria-hidden="true">
                    {icon(Icon::ChevronRight)}
                </span>
            </A>
        }
        .into_any()
    }
}

/// The newest deployments across applications; each application contributes its latest page.
#[component]
fn RecentDeployments() -> impl IntoView {
    let signals = dashboard_context().signals;
    view! {
        <section aria-labelledby="recent-deployments-heading">
            <div class="section-header">
                <h2 id="recent-deployments-heading">"Recent deployments"</h2>
            </div>
            <div class="stack-sm">
                {move || {
                    signals
                        .pagination_incomplete
                        .get()
                        .then(|| {
                            notice(
                                Tone::Warn,
                                "Application list is incomplete; some deployments may be missing.",
                            )
                        })
                }}
                {move || {
                    signals
                        .applications
                        .get()
                        .iter()
                        .filter_map(|row| {
                            row.deployment_error
                                .as_ref()
                                .map(|error| {
                                    notice(Tone::Bad, format!("{}: {error}", row.application.name))
                                })
                        })
                        .collect_view()
                }}
                <div class="table-wrap">
                    {move || {
                        let mut deployments = signals
                            .applications
                            .get()
                            .into_iter()
                            .flat_map(|row| {
                                row.deployments
                                    .into_iter()
                                    .map(move |deployment| (
                                        row.application.name.clone(),
                                        row.application.id.to_string(),
                                        deployment,
                                    ))
                            })
                            .collect::<Vec<_>>();
                        deployments
                            .sort_by(|a, b| {
                                b.2
                                    .operation
                                    .created_at_ms
                                    .cmp(&a.2.operation.created_at_ms)
                                    .then_with(|| b.2.operation.id.cmp(&a.2.operation.id))
                            });
                        if deployments.is_empty() {
                            return empty(
                                if signals.data_state.get() == DataState::Loading {
                                    "Loading deployments…"
                                } else {
                                    "No deployments yet."
                                },
                            );
                        }
                        view! {
                            <table class="table">
                                <thead>
                                    <tr>
                                        <th>"Application"</th>
                                        <th>"Status"</th>
                                        <th class="num">"Revision"</th>
                                        <th>"Phase"</th>
                                        <th>"Started"</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    {deployments
                                        .into_iter()
                                        .take(5)
                                        .map(|(name, application, deployment)| {
                                            let op = deployment.operation;
                                            view! {
                                                <tr>
                                                    <td>
                                                        <A href={format!(
                                                            "/dashboard/applications/{application}/environments/{}?deployment={}",
                                                            op.environment_id,
                                                            op.id,
                                                        )}>{name}</A>
                                                    </td>
                                                    <td>{operation_badge(op.state)}</td>
                                                    <td class="num">{format!("#{}", op.generation)}</td>
                                                    <td class="muted">
                                                        {op.phase.unwrap_or_else(|| "—".into())}
                                                    </td>
                                                    <td class="muted">{when(op.created_at_ms)}</td>
                                                </tr>
                                            }
                                        })
                                        .collect_view()}
                                </tbody>
                            </table>
                        }
                            .into_any()
                    }}
                </div>
            </div>
        </section>
    }
}
