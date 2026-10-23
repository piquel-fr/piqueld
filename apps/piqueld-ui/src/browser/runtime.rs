//! Environment runtime overview: lifecycle status, URLs, links, telemetry,
//! observed services, and diagnostics.
use super::format::timestamp;
use super::ui::{
    Icon, Tone, badge, empty, health_badge, icon, notice, operation_badge, route_name,
    telemetry_badge, when,
};
use super::{DashboardSignals, dashboard_context, release_link};
use crate::state::ApplicationHealth;
use leptos::prelude::*;
use piqueld_client::{
    Client, DiagnosticView, EnvironmentDetailView, ObservedApplicationView, ObservedServiceView,
    RouteUrl, UrlState, links::ImportState,
};

/// Live runtime detail for an environment page's Overview tab.
#[component]
pub(super) fn RuntimeOverview() -> impl IntoView {
    let dashboard = dashboard_context();
    let signals = dashboard.signals;
    let refresh = dashboard.refresh;
    view! {
        <Show when={move || signals.detail.get().is_none()}>
            <div class="card">
                <div class="stack-sm">
                    <p class="hint" role="status">
                        {move || {
                            signals
                                .detail_error
                                .get()
                                .map_or_else(
                                    || "Loading runtime detail…".into(),
                                    |error| format!("Runtime detail unavailable: {error}"),
                                )
                        }}
                    </p>
                    <div class="btn-group">
                        <button
                            type="button"
                            class="btn btn-sm"
                            disabled={move || signals.detail_loading.get()}
                            on:click={move |_| refresh.run(())}
                        >
                            {icon(Icon::Refresh)}
                            "Retry"
                        </button>
                    </div>
                </div>
            </div>
        </Show>
        {move || {
            signals
                .detail
                .get()
                .map(|detail| detail_view(&detail, signals, dashboard.client.clone()))
        }}
    }
}

/// Loaded detail: a stale-data warning if the last detail refresh failed, and
/// the runtime status, URLs, links, observed services, and diagnostics cards.
fn detail_view(
    detail: &EnvironmentDetailView,
    signals: DashboardSignals,
    client: Client,
) -> AnyView {
    view! {
        <div class="stack">
            {move || {
                signals
                    .detail_error
                    .get()
                    .map(|message| {
                        notice(
                            Tone::Warn,
                            format!(
                                "Showing the last successful detail; the latest refresh failed: {message}",
                            ),
                        )
                    })
            }} {runtime_status_card(detail, signals, client)} {urls_card(detail.urls.as_deref())}
            {links_card(detail)} {telemetry_card(detail)} {observed_card(&detail.observed)}
            {diagnostics_card(&detail.diagnostics)}
        </div>
    }
    .into_any()
}

/// The environment's lifecycle state, latest operation, release, and
/// identity, with a button that reloads only this application's detail.
#[expect(clippy::too_many_lines, reason = "flat view! markup")]
fn runtime_status_card(
    detail: &EnvironmentDetailView,
    signals: DashboardSignals,
    client: Client,
) -> AnyView {
    let refresh_detail = {
        let id = detail.application.application.id().to_string();
        move || signals.load_detail(client.clone(), id.clone())
    };
    let app = &detail.application;
    let status = &detail.status;
    let observed = &detail.observed;
    let health = ApplicationHealth::from_server_state(status.state)
        .unless_stopped(detail.environment.stopped());
    let latest = detail.latest_operation.clone();
    view! {
        <section class="card" aria-labelledby="runtime-status-heading">
            <header>
                <div>
                    <h3 id="runtime-status-heading">"Runtime status"</h3>
                    <p>
                        {status
                            .message
                            .clone()
                            .unwrap_or_else(|| "Observed from Docker Swarm.".into())}
                    </p>
                </div>
                <button
                    type="button"
                    class="btn btn-sm"
                    disabled={move || signals.detail_loading.get()}
                    on:click={move |_| refresh_detail()}
                >
                    {icon(Icon::Refresh)}
                    {move || {
                        if signals.detail_loading.get() { "Refreshing…" } else { "Refresh" }
                    }}
                </button>
            </header>
            <dl class="kv">
                <dt>"State"</dt>
                <dd>
                    {if detail.environment.stopped() {
                        health_badge(health)
                    } else {
                        badge(health.tone(), status.state.to_string())
                    }}
                </dd>
                <dt>"Runtime health"</dt>
                <dd>
                    <span class="btn-group">
                        <span>
                            {status.runtime_health.clone().unwrap_or_else(|| "unknown".into())}
                        </span>
                        {status
                            .health_observed_at_ms
                            .map(|at| view! { <span class="muted">"observed " {when(at)}</span> })}
                    </span>
                </dd>
                <dt>"Latest operation"</dt>
                <dd>
                    {latest
                        .map_or_else(
                            || "None".into_any(),
                            |op| {
                                view! {
                                    <span class="btn-group">
                                        {operation_badge(op.state)}
                                        <span>
                                            {format!("{} #{}", op.kind.as_str(), op.generation)}
                                        </span>
                                        {op
                                            .phase
                                            .map(|phase| view! { <span class="muted">{phase}</span> })}
                                        <span class="muted">{when(op.updated_at_ms)}</span>
                                    </span>
                                }
                                    .into_any()
                            },
                        )}
                </dd>
                <dt>"Release"</dt>
                <dd>
                    {detail
                        .release
                        .as_ref()
                        .map_or_else(
                            || "None".into_any(),
                            |release| {
                                release_link(app.application.id().as_str(), &release.to_string())
                            },
                        )}
                </dd>
                <dt>"Generation"</dt>
                <dd>
                    {format!(
                        "{} saved · {} resolved",
                        app.generation,
                        detail
                            .environment
                            .resolved_generation
                            .map_or_else(|| "none".to_owned(), |value| value.to_string()),
                    )}
                </dd>
                <dt>"Resources"</dt>
                <dd>
                    {format!(
                        "{} network{} · {} volume{}",
                        observed.network_count,
                        if observed.network_count == 1 { "" } else { "s" },
                        observed.volume_count,
                        if observed.volume_count == 1 { "" } else { "s" },
                    )}
                </dd>
                <dt>"Environment"</dt>
                <dd>
                    {detail.environment.name.to_string()} " "
                    <code>{detail.environment.id.to_string()}</code>
                </dd>
                <dt>"Application ID"</dt>
                <dd>
                    <code>{app.application.id().to_string()}</code>
                </dd>
                <dt>"Created"</dt>
                <dd>{timestamp(app.created_at_ms)}</dd>
                <dt>"Updated"</dt>
                <dd>{timestamp(app.updated_at_ms)}</dd>
            </dl>
        </section>
    }
    .into_any()
}

/// The routes of the current runtime target, or why there are none.
fn urls_card(urls: Option<&[RouteUrl]>) -> AnyView {
    view! {
        <section class="card card-flush" aria-labelledby="urls-title">
            <header>
                <div>
                    <h3 id="urls-title">"URLs"</h3>
                    <p>"Routes of the current runtime target, and whether each is reachable."</p>
                </div>
            </header>
            {match urls {
                None => empty("This daemon does not report URLs."),
                Some([]) => empty("No routes are deployed in this environment."),
                Some(urls) => {
                    view! {
                        <table class="table">
                            <thead>
                                <tr>
                                    <th>"URL"</th>
                                    <th>"Name"</th>
                                    <th>"Visibility"</th>
                                    <th>"Destination"</th>
                                    <th>"State"</th>
                                    <th>"Details"</th>
                                </tr>
                            </thead>
                            <tbody>{urls.iter().map(url_row).collect_view()}</tbody>
                        </table>
                    }
                        .into_any()
                }
            }}
        </section>
    }
    .into_any()
}

/// The services Docker reports for the current runtime target.
fn observed_card(observed: &ObservedApplicationView) -> AnyView {
    view! {
        <section class="card card-flush" aria-labelledby="observed-title">
            <header>
                <div>
                    <h3 id="observed-title">"Observed services"</h3>
                    <p>
                        "Replicas and health as reported by Docker for the current runtime target."
                    </p>
                </div>
            </header>
            {if observed.services.is_empty() {
                empty("No services are running in this environment.")
            } else {
                view! {
                    <table class="table">
                        <thead>
                            <tr>
                                <th>"Service"</th>
                                <th>"Image"</th>
                                <th class="num">"Healthy"</th>
                                <th>"Health"</th>
                            </tr>
                        </thead>
                        <tbody>
                            {observed.services.iter().map(observed_service_row).collect_view()}
                        </tbody>
                    </table>
                }
                    .into_any()
            }}
        </section>
    }
    .into_any()
}

/// Reconciliation problems of the environment.
fn diagnostics_card(diagnostics: &[DiagnosticView]) -> AnyView {
    view! {
        <section class="card" aria-labelledby="diagnostics-title">
            <header>
                <div>
                    <h3 id="diagnostics-title">"Diagnostics"</h3>
                    <p>
                        "Reconciliation problems from status, runtime observation, and the latest operation."
                    </p>
                </div>
            </header>
            {if diagnostics.is_empty() {
                view! { <p class="hint">"No diagnostics reported."</p> }.into_any()
            } else {
                view! {
                    <ul class="stack-sm">
                        {diagnostics.iter().map(diagnostic_view).collect_view()}
                    </ul>
                }
                    .into_any()
            }}
        </section>
    }
    .into_any()
}

/// Table row for one URL: a link with its name, state, and what keeps it
/// pending. A wildcard's URL stands for many names, so it is not a link.
fn url_row(url: &RouteUrl) -> AnyView {
    let tone = match url.state {
        UrlState::Ready => Tone::Ok,
        UrlState::Pending => Tone::Pending,
    };
    let link = if url.url.contains('*') {
        view! { <code>{url.url.clone()}</code> }.into_any()
    } else {
        view! {
            <a href={url.url.clone()} target="_blank" rel="noopener noreferrer">
                {url.url.clone()}
            </a>
        }
        .into_any()
    };
    view! {
        <tr>
            <td>{link}</td>
            <td>{route_name(url.name.clone())}</td>
            <td>{url.visibility.to_string()}</td>
            <td>
                <code>{url.target.to_string()}</code>
            </td>
            <td>{badge(tone, url.state.to_string())}</td>
            <td class="muted">{waiting_for(url)}</td>
        </tr>
    }
    .into_any()
}

/// The environment's imports and whether each connects, and the environments
/// importing its services; nothing when it takes part in no link.
fn links_card(detail: &EnvironmentDetailView) -> Option<AnyView> {
    let links = detail
        .links
        .as_ref()
        .filter(|links| !links.imports.is_empty() || !links.importers.is_empty())?;
    let imports = links.imports.iter().map(|import| {
        let (tone, state) = match import.state {
            ImportState::Connected => (Tone::Ok, "connected"),
            ImportState::Unavailable => (Tone::Pending, "not exported to this environment"),
            ImportState::Full => (Tone::Pending, "exporter links to too many environments"),
        };
        view! {
            <tr>
                <td>"Imports"</td>
                <td>
                    <code>{import.alias.clone()}</code>
                </td>
                <td>{format!("{} {}", import.application, import.environment)}</td>
                <td>{badge(tone, state)}</td>
            </tr>
        }
    });
    let importers = links.importers.iter().map(|importer| {
        view! {
            <tr>
                <td>"Imported by"</td>
                <td>
                    <code>{importer.service.to_string()}</code>
                </td>
                <td>{format!("{} {}", importer.application, importer.environment)}</td>
                <td>{badge(Tone::Ok, "connected")}</td>
            </tr>
        }
    });
    Some(
        view! {
            <section class="card card-flush" aria-labelledby="links-title">
                <header>
                    <div>
                        <h3 id="links-title">"Links"</h3>
                        <p>
                            "Services this environment reaches in other applications, and the environments reaching its exported services."
                        </p>
                    </div>
                </header>
                <table class="table">
                    <thead>
                        <tr>
                            <th>"Direction"</th>
                            <th>"Service"</th>
                            <th>"Environment"</th>
                            <th>"State"</th>
                        </tr>
                    </thead>
                    <tbody>{imports.collect_view()} {importers.collect_view()}</tbody>
                </table>
            </section>
        }
        .into_any(),
    )
}

/// Whether the environment is granted telemetry, which service receives it,
/// and whether the daemon is connected to it; nothing when it is neither
/// granted nor deploys a receiver.
pub(super) fn telemetry_card(detail: &EnvironmentDetailView) -> Option<AnyView> {
    let telemetry = detail
        .telemetry
        .as_ref()
        .filter(|telemetry| telemetry.granted || telemetry.receiver.is_some())?;
    let granted = if telemetry.granted {
        badge(Tone::Ok, "granted")
    } else {
        badge(Tone::Warn, "not granted")
    };
    let receiver = telemetry.receiver.as_ref().map_or_else(
        || "No deployed service receives it.".to_owned(),
        |service| format!("{service} receives it on /run/piqueld/telemetry/otlp.sock."),
    );
    let connection = match (&telemetry.destination, telemetry.granted) {
        (Some(destination), _) => telemetry_badge(destination),
        (None, true) if telemetry.receiver.is_some() => badge(Tone::Pending, "not connected yet"),
        (None, _) => badge(Tone::Neutral, "not exported"),
    };
    Some(
        view! {
            <section class="card" aria-labelledby="telemetry-title">
                <header>
                    <div>
                        <h3 id="telemetry-title">"Telemetry"</h3>
                        <p>
                            "A granted environment's receiving service gets the daemon's traces, metrics and logs from every application."
                        </p>
                    </div>
                </header>
                <dl class="kv">
                    <dt>"Grant"</dt>
                    <dd>{granted}</dd>
                    <dt>"Receiver"</dt>
                    <dd>{receiver}</dd>
                    <dt>"Export"</dt>
                    <dd>{connection}</dd>
                </dl>
            </section>
        }
        .into_any(),
    )
}

/// What keeps a URL pending, e.g. `Waiting for: ingress, DNS pending`, or
/// nothing once it is ready.
fn waiting_for(url: &RouteUrl) -> Option<String> {
    (!url.pending.is_empty()).then(|| format!("Waiting for: {}", url.waiting_for()))
}

/// Table row for one observed service, followed by a row of its diagnostics.
fn observed_service_row(service: &ObservedServiceView) -> AnyView {
    let health = ApplicationHealth::from_convergence(&service.convergence);
    let diagnostics = service
        .diagnostics
        .iter()
        .map(diagnostic_view)
        .collect_view();
    view! {
        <tr>
            <td>
                <strong>{service.name.clone()}</strong>
            </td>
            <td class="muted">
                {service
                    .image
                    .clone()
                    .map_or_else(
                        || "Not observed".into_any(),
                        |image| view! { <code>{image}</code> }.into_any(),
                    )}
            </td>
            <td class="num">
                {format!("{} / {}", service.healthy_replicas, service.desired_replicas)}
            </td>
            <td>{health_badge(health)}</td>
        </tr>
        {(!service.diagnostics.is_empty())
            .then(|| {
                view! {
                    <tr>
                        <td colspan="4">
                            <ul class="stack-sm">{diagnostics}</ul>
                        </td>
                    </tr>
                }
            })}
    }
    .into_any()
}

/// List item for one diagnostic code and message.
fn diagnostic_view(diagnostic: &DiagnosticView) -> AnyView {
    view! {
        <li>
            {notice(
                Tone::Warn,
                view! {
                    <span class="btn-group">
                        {badge(Tone::Warn, diagnostic.code.clone())}
                        <span>{diagnostic.message.clone()}</span>
                    </span>
                },
            )}
        </li>
    }
    .into_any()
}
