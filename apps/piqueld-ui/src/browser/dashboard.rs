//! Sidebar navigation, overview metrics, application directory, and recent deployments.
use super::format::timestamp;
use super::ui::{
    Icon, PageHeader, Tone, badge, empty, health_badge, icon, key_values, loading, metric, notice,
    operation_badge, when,
};
use super::{ApplicationRow, ClientErrorExt, access::can, dashboard_context, management};
use crate::overview::PreviewLoad;
use crate::state::{ApplicationHealth, ConnectionState, DataState};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::use_location;
use piqueld_client::{
    Client, PreviewUsage,
    access::{GlobalPermission, Permission},
    observability::Bytes,
    system::{
        CertificateStatus, DependencyStatus, DnsProviderStatus, ImageStatus, PublicIngressStatus,
        ReadinessStatus, TelemetryStatus,
    },
};

impl PreviewLoad {
    /// Tone of the previews card.
    const fn tone(self) -> Tone {
        match self {
            Self::Within => Tone::Ok,
            Self::AtLimit => Tone::Warn,
            Self::OverLimit => Tone::Bad,
        }
    }
}

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
/// sign-out button. On narrow screens it is a bar whose menu button opens
/// them, closing again on navigation.
#[component]
pub(super) fn Sidebar() -> impl IntoView {
    let signals = dashboard_context().signals;
    let who = super::auth::signed_in();
    let opened = RwSignal::new(false);
    let location = use_location();
    Effect::new(move |_| {
        location.pathname.track();
        opened.set(false);
    });
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
        <aside class="sidebar" data-open={move || opened.get().then_some("")}>
            <A attr:class="brand" href="/dashboard/">
                <span class="brand-mark" aria-hidden="true">
                    "p"
                </span>
                "piqueld"
            </A>
            <button
                type="button"
                class="btn btn-sm nav-toggle"
                aria-expanded={move || opened.get().to_string()}
                aria-controls="sidebar-drawer"
                on:click={move |_| opened.update(|opened| *opened = !*opened)}
            >
                {icon(Icon::Menu)}
                "Menu"
            </button>
            <div class="sidebar-drawer" id="sidebar-drawer">
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
                            {nav_link(
                                "/dashboard/settings",
                                Icon::Settings,
                                "Host settings",
                                false,
                            )}
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

/// Daemon connectivity and dependency readiness, one row each: the daemon,
/// its database, Docker and Swarm, ingress, DNS providers, certificates,
/// previews, built images and telemetry export, with a button that triggers
/// a manual dashboard refresh and, when DNS providers are configured and the
/// caller holds `system:operate`, one that checks their credentials now.
/// Each row says its state in a line; the rest opens on demand.
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
        <section class="card card-flush" aria-labelledby="system-readiness-heading">
            <header>
                <div>
                    <h3 id="system-readiness-heading">"System status"</h3>
                    <p>"The daemon and what it needs to deploy and serve applications."</p>
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
            <div aria-live="polite">
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
                <ul class="status-list">
                    {move || connection_row(signals.connection.get())}
                    {move || {
                        signals
                            .system
                            .get()
                            .filter(|system| system.tailscale.enabled)
                            .map(|system| {
                                health_row(
                                    "Tailnet node",
                                    system.tailscale.healthy,
                                    system.tailscale.message,
                                )
                            })
                    }} {move || signals.readiness.get().map(|status| readiness_rows(&status))}
                    {move || {
                        signals
                            .system
                            .get()
                            .map(|system| {
                                system.dns.providers.iter().map(dns_provider_row).collect_view()
                            })
                    }}
                    {move || {
                        signals
                            .system
                            .get()
                            .and_then(|system| certificates_row(&system.dns.certificates))
                    }}
                    {move || {
                        signals.system.get().map(|system| preview_row(system.previews.as_ref()))
                    }} {move || signals.system.get().map(|system| images_row(&system.images))}
                    {move || signals.system.get().map(|system| telemetry_rows(&system.telemetry))}
                </ul>
            </div>
        </section>
    }
}

/// One component's row: its `name`, a `tone`d `label`, and a `summary` of
/// a line. With `details`, the row opens to show them.
fn status_row(
    name: impl Into<String>,
    tone: Tone,
    label: impl Into<String>,
    summary: impl Into<String>,
    details: Option<AnyView>,
) -> AnyView {
    let (name, summary) = (name.into(), summary.into());
    let head = view! {
        <strong>{name}</strong>
        {badge(tone, label)}
        <span class="status-summary">{summary}</span>
    };
    match details {
        Some(details) => view! {
            <li>
                <details class="status-more">
                    <summary>
                        <span class="status-row" data-tone={tone.attr()}>
                            {head}
                            <span class="chevron" aria-hidden="true">
                                {icon(Icon::ChevronRight)}
                            </span>
                        </span>
                    </summary>
                    <div class="status-details">{details}</div>
                </details>
            </li>
        }
        .into_any(),
        None => view! {
            <li class="status-row" data-tone={tone.attr()}>
                {head}
            </li>
        }
        .into_any(),
    }
}

/// Rows for the daemon's dependencies and its ingress: the HTTPS gateway,
/// its Cloudflare Tunnel in tunnel mode, and its private listener.
fn readiness_rows(status: &ReadinessStatus) -> AnyView {
    let ingress = &status.ingress;
    let (tone, label) = if !ingress.healthy {
        (Tone::Bad, "Unhealthy")
    } else if !ingress.enabled {
        (Tone::Neutral, "Disabled")
    } else {
        (Tone::Ok, "Ready")
    };
    // In tunnel mode, public routes' CNAME records point at the tunnel ID.
    let tunnel_row = match &ingress.public {
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
            Some(status_row(
                "Cloudflare Tunnel",
                tone,
                label,
                format!("Tunnel {id}. {message}"),
                None,
            ))
        }
        _ => None,
    };
    // The apps node's name and addresses are what private routes' DNS
    // records point at.
    let private = &ingress.private;
    let private_row = (ingress.enabled && private.enabled).then(|| {
        let node = match (&private.dns_name, private.addresses.is_empty()) {
            (Some(name), false) => format!("{name} ({}). ", private.addresses.join(", ")),
            _ => String::new(),
        };
        health_row(
            "Private listener",
            private.healthy,
            format!("{node}{}", private.message),
        )
    });
    view! {
        {dependency_row("Database", &status.database)}
        {dependency_row("Docker Engine", &status.docker)}
        {dependency_row("Swarm manager", &status.swarm)}
        {status_row("HTTPS ingress", tone, label, ingress.message.clone(), None)}
        {tunnel_row}
        {private_row}
    }
    .into_any()
}

/// Row for one DNS provider: its kind, discovered zones, health and whether
/// it manages route records.
fn dns_provider_row(provider: &DnsProviderStatus) -> AnyView {
    let zones = if provider.zones.is_empty() {
        "no zones".to_owned()
    } else {
        provider.zones.join(", ")
    };
    health_row(
        format!("DNS provider ({})", provider.kind),
        provider.healthy,
        format!(
            "{zones}. {}. {}",
            provider.message,
            if provider.manage_records {
                "Manages route DNS records"
            } else {
                "Route DNS records are manual"
            }
        ),
    )
}

/// One row for every DNS-01 certificate, toned by the worst of them, counting
/// how many are issued, pending, or failing; each one's name, hostnames,
/// expiry and last error open on demand. Nothing without certificates.
fn certificates_row(certificates: &[CertificateStatus]) -> Option<AnyView> {
    if certificates.is_empty() {
        return None;
    }
    let state =
        |certificate: &CertificateStatus| match (&certificate.error, certificate.expires_at_ms) {
            (Some(_), None) => (Tone::Bad, "failed"),
            (Some(_), Some(_)) => (Tone::Warn, "renewal failing"),
            (None, Some(_)) => (Tone::Ok, "issued"),
            (None, None) => (Tone::Pending, "pending"),
        };
    let count = |label: &str| {
        certificates
            .iter()
            .filter(|certificate| state(certificate).1 == label)
            .count()
    };
    let counts = ["issued", "pending", "renewal failing", "failed"]
        .into_iter()
        .filter_map(|label| {
            let count = count(label);
            (count > 0).then(|| format!("{count} {label}"))
        })
        .collect::<Vec<_>>()
        .join(", ");
    let worst = certificates
        .iter()
        .map(|certificate| state(certificate).0)
        .max_by_key(|tone| match tone {
            Tone::Bad => 3,
            Tone::Warn => 2,
            Tone::Pending => 1,
            _ => 0,
        })
        .unwrap_or(Tone::Ok);
    let label = match worst {
        Tone::Bad => "Failing",
        Tone::Warn => "Renewal failing",
        Tone::Pending => "Pending",
        _ => "Issued",
    };
    let rows = certificates
        .iter()
        .map(|certificate| {
            let (tone, label) = state(certificate);
            let hostnames = certificate.hostnames.join("\n");
            let expiry = certificate.expires_at_ms.map_or_else(
                || "not issued yet".to_owned(),
                |at| format!("expires {}", timestamp(at)),
            );
            view! {
                <tr>
                    <td>
                        <strong>{certificate.name.clone()}</strong>
                    </td>
                    <td>{badge(tone, label)}</td>
                    <td class="muted" title={hostnames}>
                        {match certificate.hostnames.len() {
                            0 => "no routes".to_owned(),
                            1 => certificate.hostnames[0].clone(),
                            count => format!("{count} hostnames"),
                        }}
                    </td>
                    <td class="muted">{expiry}</td>
                    <td class="muted">{certificate.error.clone()}</td>
                </tr>
            }
        })
        .collect_view();
    let details = view! {
        <table class="table">
            <thead>
                <tr>
                    <th>"Certificate"</th>
                    <th>"State"</th>
                    <th>"Routes"</th>
                    <th>"Expiry"</th>
                    <th>"Last error"</th>
                </tr>
            </thead>
            <tbody>{rows}</tbody>
        </table>
    }
    .into_any();
    Some(status_row(
        format!(
            "{} certificate{}",
            certificates.len(),
            if certificates.len() == 1 { "" } else { "s" }
        ),
        worst,
        label,
        counts,
        Some(details),
    ))
}

/// Row for previews against their `[previews]` limits: the installation's
/// count in a line, and on demand each readable application's and the limits
/// of their replicas. A count at its limit refuses new previews; one over it,
/// after the limit was lowered, keeps its existing previews running.
fn preview_row(usage: Option<&PreviewUsage>) -> AnyView {
    let Some(usage) = usage else {
        return status_row(
            "Previews",
            Tone::Neutral,
            "Unavailable",
            "The daemon could not count previews.",
            None,
        );
    };
    let limits = &usage.limits;
    let load = PreviewLoad::of(usage);
    let applications = usage.applications.iter().map(|application| {
        (
            application.name.clone(),
            format!("{} of {}", application.previews, limits.max_per_application),
        )
    });
    let unlimited = (usage.unlimited_replicas > 0).then(|| {
        format!(
            "{} replicas deployed before the limits applied run without them until redeployed.",
            usage.unlimited_replicas
        )
    });
    let details = view! {
        {key_values(applications)}
        <p class="hint">
            {format!(
                "Their replicas are limited to {} millicores and {} in total. Services without limits get {} millicores and {}, and replicas are capped at {}.",
                usage.cpu_millis,
                Bytes(usage.memory_bytes),
                limits.default_cpu_millis,
                Bytes(limits.default_memory_bytes),
                limits.max_replicas,
            )} {unlimited.map(|text| format!(" {text}"))}
        </p>
    }
    .into_any();
    status_row(
        "Previews",
        load.tone(),
        load.label(),
        format!("{} of {} previews", usage.total, limits.max_total),
        Some(details),
    )
}

/// Row for the images this installation built: how many cleanup kept, and
/// what it removed since the daemon started.
fn images_row(images: &ImageStatus) -> AnyView {
    match images.cleaned_at_ms {
        Some(at) => status_row(
            "Built images",
            Tone::Ok,
            "Cleaned up",
            format!(
                "{} kept. Cleanup removed {} since the daemon started, last {}.",
                images.images,
                Bytes(images.reclaimed_bytes),
                timestamp(at)
            ),
            None,
        ),
        None => status_row(
            "Built images",
            Tone::Pending,
            "Not cleaned up",
            "Cleanup hasn't run since the daemon started.",
            None,
        ),
    }
}

/// Rows for telemetry export: one per collector, with the receiving service
/// and last export in a line and the endpoint, dropped records and exported
/// signals on demand, or one saying export is off. Headers appear only as
/// the file they were read from.
fn telemetry_rows(telemetry: &TelemetryStatus) -> AnyView {
    if telemetry.destinations.is_empty() {
        return status_row(
            "Telemetry export",
            Tone::Neutral,
            "Off",
            "Set an endpoint under [telemetry] in the daemon configuration, or grant an environment with a receiving service telemetry, to export traces, metrics and logs over OTLP.",
            None,
        );
    }
    telemetry
        .destinations
        .iter()
        .map(|destination| {
            let receiver = destination.receiver.as_ref().map_or_else(
                || "the configured collector".to_owned(),
                |receiver| match (&receiver.application, &receiver.environment) {
                    (Some(application), Some(environment)) => {
                        format!("{application} {environment}")
                    }
                    _ => receiver.environment_id.to_string(),
                },
            );
            let last = destination.last_success_ms.map_or_else(
                || "nothing exported yet".to_owned(),
                |at| format!("last export {}", super::format::relative(at)),
            );
            let details = key_values([
                ("Endpoint", destination.endpoint.clone()),
                (
                    "Dropped records",
                    format!("{} since the daemon started", destination.dropped),
                ),
                (
                    "Headers",
                    destination.headers.clone().unwrap_or_else(|| "none".into()),
                ),
                ("Exports", telemetry.exported().to_owned()),
            ])
            .into_any();
            status_row(
                "Telemetry export",
                super::ui::telemetry_tone(destination),
                destination.state(),
                format!("To {receiver}, {last}."),
                Some(details),
            )
        })
        .collect_view()
        .into_any()
}

/// Row for the browser's connection to the daemon itself.
fn connection_row(state: ConnectionState) -> AnyView {
    let message = match state {
        ConnectionState::Loading => "Waiting for the daemon",
        ConnectionState::Reachable => "The API is responding",
        ConnectionState::Failed => "The daemon returned an error",
        ConnectionState::Unreachable => "The dashboard cannot connect to piqueld",
    };
    status_row("piqueld daemon", state.tone(), state.label(), message, None)
}

/// Row for one daemon dependency.
fn dependency_row(name: &'static str, status: &DependencyStatus) -> AnyView {
    match status {
        DependencyStatus::Ready => status_row(name, Tone::Ok, "Ready", "Available", None),
        DependencyStatus::Failed { message } => {
            status_row(name, Tone::Bad, "Failed", message.clone(), None)
        }
    }
}

/// Row for a component that is either ready or unhealthy.
fn health_row(name: impl Into<String>, healthy: bool, message: impl Into<String>) -> AnyView {
    if healthy {
        status_row(name, Tone::Ok, "Ready", message, None)
    } else {
        status_row(name, Tone::Bad, "Unhealthy", message, None)
    }
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
                    DataState::Loading => loading("Loading applications…"),
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
        let subtitle = self.subtitle();
        let latest = self.last_deployed_ms();
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
                        signals
                            .applications
                            .with(|rows| {
                                let deployments = ApplicationRow::recent_deployments(rows, 5);
                                if deployments.is_empty() {
                                    return if signals.data_state.get() == DataState::Loading {
                                        loading("Loading deployments…")
                                    } else {
                                        empty("No deployments yet.")
                                    };
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
                                                .map(|(application, deployment)| {
                                                    let op = &deployment.operation;
                                                    let href = format!(
                                                        "/dashboard/applications/{}/environments/{}?deployment={}",
                                                        application.id,
                                                        op.environment_id,
                                                        op.id,
                                                    );
                                                    let name = application.name.clone();
                                                    view! {
                                                        <tr>
                                                            <td>
                                                                <A href={href}>{name}</A>
                                                            </td>
                                                            <td>{operation_badge(op.state)}</td>
                                                            <td class="num">{format!("#{}", op.generation)}</td>
                                                            <td class="muted">
                                                                {op.phase.clone().unwrap_or_else(|| "—".into())}
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
                            })
                    }}
                </div>
            </div>
        </section>
    }
}
