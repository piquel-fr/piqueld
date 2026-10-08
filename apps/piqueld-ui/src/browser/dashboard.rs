//! Sidebar navigation, overview metrics, application directory, and recent deployments.
use super::ui::{
    Icon, PageHeader, Tone, badge, empty, health_badge, icon, metric, notice, operation_badge, when,
};
use super::{
    ApplicationRow, client_error_message, connection_label, dashboard_context, management,
    row_health,
};
use crate::state::{ApplicationHealth, ConnectionState, DataState};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use piqueld_client::{
    Client,
    system::{CertificateStatus, DependencyStatus, DnsProviderStatus, PublicIngressStatus},
};

#[component]
pub(super) fn Sidebar() -> impl IntoView {
    let signals = dashboard_context().signals;
    let who = super::auth::signed_in();
    // Pages the caller could not load anything on are hidden; the daemon
    // enforces every permission regardless.
    let system = super::access::can(piqueld_client::access::Permission::Global(
        piqueld_client::access::GlobalPermission::SystemRead,
    ));
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
                    <Show when={move || system.get()}>
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
                    <Show when={move || system.get()}>
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
                        data-tone={move || connection_tone(signals.connection.get())}
                    ></span>
                    {move || connection_label(signals.connection.get())}
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

fn nav_link(href: &'static str, glyph: Icon, label: &'static str, exact: bool) -> AnyView {
    view! {
        <A href={href} attr:class="nav-link" exact={exact}>
            {icon(glyph)}
            {label}
        </A>
    }
    .into_any()
}

const fn connection_tone(state: ConnectionState) -> &'static str {
    match state {
        ConnectionState::Loading => "pending",
        ConnectionState::Reachable => "ok",
        ConnectionState::Failed | ConnectionState::Unreachable => "bad",
    }
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
            .filter(|row| predicate(row_health(row)))
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
pub(super) fn ReadinessPanel() -> impl IntoView {
    let context = dashboard_context();
    let signals = context.signals;
    let refresh = context.refresh;
    let operate = super::access::can(piqueld_client::access::Permission::Global(
        piqueld_client::access::GlobalPermission::SystemOperate,
    ));
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
                Err(error) => dns_error.set(Some(client_error_message(&error))),
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
                                system.as_ref().is_some_and(|system| !system.dns.providers.is_empty())
                            });
                        (providers && operate.get())
                            .then(|| {
                                view! {
                                    <button
                                        type="button"
                                        class="btn btn-sm"
                                        disabled={move || checking_dns.get()}
                                        on:click=check_dns
                                    >
                                        {icon(Icon::Key)}
                                        {move || {
                                            if checking_dns.get() { "Checking…" } else { "Check DNS providers" }
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
                        .map(|error| notice(Tone::Bad, format!("DNS provider check failed: {error}")))
                }}
                <div class="status-grid">
                    {move || connection_readiness(signals.connection.get())}
                    {move || {
                        signals.system.get().filter(|system| system.tailscale.enabled).map(|system| {
                            let tailnet = system.tailscale;
                            let (tone, label) = if tailnet.healthy {
                                (Tone::Ok, "Ready")
                            } else {
                                (Tone::Bad, "Unhealthy")
                            };
                            status_card("Tailnet node", tone, label, &tailnet.message)
                        })
                    }}
                    {move || {
                        signals.system.get().map(|system| {
                            system
                                .dns
                                .providers
                                .iter()
                                .map(dns_provider_card)
                                .chain(system.dns.certificates.iter().map(certificate_card))
                                .collect_view()
                        })
                    }}
                    {move || {
                        signals
                            .readiness
                            .get()
                            .map(|status| {
                                let ingress = &status.ingress;
                                let (tone, label) = if !ingress.healthy {
                                    (Tone::Bad, "Unhealthy")
                                } else if !ingress.enabled {
                                    (Tone::Neutral, "Disabled")
                                } else {
                                    (Tone::Ok, "Ready")
                                };
                                // The apps node's name and addresses are what private
                                // routes' DNS records point at.
                                let private = &ingress.private;
                                let private_card = (ingress.enabled && private.enabled)
                                    .then(|| {
                                        let (tone, label) = if private.healthy {
                                            (Tone::Ok, "Ready")
                                        } else {
                                            (Tone::Bad, "Unhealthy")
                                        };
                                        let node = match (&private.dns_name, private.addresses.is_empty()) {
                                            (Some(name), false) => {
                                                format!("{name} ({}). ", private.addresses.join(", "))
                                            }
                                            _ => String::new(),
                                        };
                                        status_card(
                                            "Private listener",
                                            tone,
                                            label,
                                            &format!("{node}{}", private.message),
                                        )
                                    });
                                // In tunnel mode, public routes' CNAME records point
                                // at the tunnel ID.
                                let tunnel_card = match &ingress.public {
                                    PublicIngressStatus::Tunnel { id, connections, message }
                                        if ingress.enabled =>
                                    {
                                        let (tone, label) = if *connections > 0 {
                                            (Tone::Ok, "Connected")
                                        } else {
                                            (Tone::Bad, "Disconnected")
                                        };
                                        Some(status_card(
                                            "Cloudflare Tunnel",
                                            tone,
                                            label,
                                            &format!("Tunnel {id}, {connections} edge connections. {message}"),
                                        ))
                                    }
                                    _ => None,
                                };
                                view! {
                                    {dependency_readiness("Database", status.database)}
                                    {dependency_readiness("Docker Engine", status.docker)}
                                    {dependency_readiness("Swarm manager", status.swarm)}
                                    {status_card("HTTPS ingress", tone, label, &ingress.message)}
                                    {tunnel_card}
                                    {private_card}
                                }
                            })
                    }}
                </div>
            </div>
        </section>
    }
}

/// Card for one DNS provider: its kind, discovered zones and health.
fn dns_provider_card(provider: &DnsProviderStatus) -> AnyView {
    let (tone, label) = if provider.healthy {
        (Tone::Ok, "Ready")
    } else {
        (Tone::Bad, "Unhealthy")
    };
    let zones = if provider.zones.is_empty() {
        "no zones".to_owned()
    } else {
        provider.zones.join(", ")
    };
    status_card(
        "DNS provider",
        tone,
        label,
        &format!("{}: {zones}. {}", provider.kind, provider.message),
    )
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
        |at| format!("expires {}", super::format::timestamp(at)),
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
fn connection_readiness(state: ConnectionState) -> AnyView {
    let (tone, message) = match state {
        ConnectionState::Loading => (Tone::Pending, "Waiting for the daemon"),
        ConnectionState::Reachable => (Tone::Ok, "The API is responding"),
        ConnectionState::Failed => (Tone::Bad, "The daemon returned an error"),
        ConnectionState::Unreachable => (Tone::Bad, "The dashboard cannot connect to piqueld"),
    };
    status_card("piqueld daemon", tone, connection_label(state), message)
}

/// Readiness card for one daemon dependency.
fn dependency_readiness(name: &'static str, status: DependencyStatus) -> AnyView {
    let (tone, label, message) = match status {
        DependencyStatus::Ready => (Tone::Ok, "Ready", "Available".to_owned()),
        DependencyStatus::Failed { message } => (Tone::Bad, "Failed", message),
    };
    status_card(name, tone, label, &message)
}

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
                {move || signals.applications.get().into_iter().map(application_row).collect_view()}
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

fn application_row(row: ApplicationRow) -> AnyView {
    let health = row_health(&row);
    let subtitle = if row.application.delete_intent {
        "Deletion requested".to_owned()
    } else {
        row.message()
            .unwrap_or_else(|| format!("Generation {}", row.application.generation))
    };
    let latest = row
        .deployments
        .iter()
        .max_by_key(|deployment| deployment.operation.created_at_ms)
        .map(|deployment| deployment.operation.created_at_ms);
    view! {
        <A attr:class="list-row" href={format!("/dashboard/applications/{}", row.application.id)}>
            <span class="app-icon" aria-hidden="true">
                {icon(Icon::Package)}
            </span>
            <span class="title">{row.application.name.clone()} <small>{subtitle}</small></span>
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
