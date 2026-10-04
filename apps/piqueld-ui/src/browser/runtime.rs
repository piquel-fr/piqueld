//! Application runtime overview: lifecycle status, observed services, and diagnostics.
use super::format::timestamp;
use super::ui::{Icon, Tone, badge, empty, health_badge, icon, notice, operation_badge, when};
use super::{DashboardSignals, dashboard_context, load_detail};
use crate::state::ApplicationHealth;
use leptos::prelude::*;
use piqueld_client::{Client, DiagnosticView, EnvironmentDetailView, ObservedServiceView};

/// Live runtime detail for the application editor's Overview tab.
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

/// Loaded detail: a stale-data warning if the last detail refresh failed, the
/// runtime status, observed services, and diagnostics cards, and a button that
/// reloads only this application's detail.
fn detail_view(
    detail: &EnvironmentDetailView,
    signals: DashboardSignals,
    client: Client,
) -> AnyView {
    let refresh_detail = {
        let id = detail.application.application.id().to_string();
        move || load_detail(client.clone(), signals, id.clone())
    };
    let app = &detail.application;
    let status = &detail.status;
    let observed = &detail.observed;
    let health = ApplicationHealth::from_server_state(status.state);
    let latest = detail.latest_operation.clone();
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
            }} <section class="card" aria-labelledby="runtime-status-heading">
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
                    <dd>{badge(health.tone(), status.state.to_string())}</dd>
                    <dt>"Runtime health"</dt>
                    <dd>{status.runtime_health.clone().unwrap_or_else(|| "unknown".into())}</dd>
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
            </section> <section class="card card-flush" aria-labelledby="observed-title">
                <header>
                    <div>
                        <h3 id="observed-title">"Observed services"</h3>
                        <p>
                            "Replicas and health as reported by Docker for the current runtime target."
                        </p>
                    </div>
                </header>
                {if observed.services.is_empty() {
                    empty("No services are running for this application.")
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
            </section> <section class="card" aria-labelledby="diagnostics-title">
                <header>
                    <div>
                        <h3 id="diagnostics-title">"Diagnostics"</h3>
                        <p>
                            "Reconciliation problems from status, runtime observation, and the latest operation."
                        </p>
                    </div>
                </header>
                {if detail.diagnostics.is_empty() {
                    view! { <p class="hint">"No diagnostics reported."</p> }.into_any()
                } else {
                    view! {
                        <ul class="stack-sm">
                            {detail.diagnostics.iter().map(diagnostic_view).collect_view()}
                        </ul>
                    }
                        .into_any()
                }}
            </section>
        </div>
    }
    .into_any()
}

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
