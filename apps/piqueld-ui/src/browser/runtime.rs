//! Application runtime details and diagnostics.
use super::{DashboardSignals, dashboard_context, health_class, load_detail};
use crate::state::ApplicationHealth;
use leptos::{CollectView, IntoView, Show, SignalGet, View, component, view};
use piqueld_client::{ApplicationDetailView, Client, DiagnosticView, ObservedServiceView};

#[derive(Clone, Copy)]
pub(super) enum RuntimeSection {
    Overview,
    Services,
    Diagnostics,
}

#[component]
pub(super) fn RuntimeDetails(section: RuntimeSection) -> impl IntoView {
    let dashboard = dashboard_context();
    let signals = dashboard.signals;
    let refresh = leptos::store_value(dashboard.refresh);
    let retry = move |_| refresh.with_value(|refresh| refresh());
    view! {
        <Show when={move || signals.detail.get().is_none()}>
            <p role="status">
                {move || {
                    signals
                        .detail_error
                        .get()
                        .map_or_else(
                            || "Loading runtime detail…".into(),
                            |error| format!("Detail unavailable: {error}"),
                        )
                }}
            </p>
            <button disabled={move || signals.detail_loading.get()} on:click={retry}>
                "Retry runtime detail"
            </button>
        </Show>
        {move || {
            signals
                .detail
                .get()
                .map(|detail| detail_view(&detail, signals, dashboard.client.clone(), section))
        }}
    }
}

fn detail_view(
    detail: &ApplicationDetailView,
    signals: DashboardSignals,
    client: Client,
    section: RuntimeSection,
) -> View {
    let refresh_detail = {
        let id = detail.application.application.id().to_string();
        move || load_detail(client.clone(), signals, id.clone())
    };
    view! {
        <div class="grid gap-4">
            {move || {
                signals
                    .detail_error
                    .get()
                    .map(|message| {
                        view! {
                            <p
                                class="rounded-lg border border-warn bg-warn-bg p-3 text-warn"
                                role="status"
                            >
                                {format!(
                                    "Showing the last successful detail; the latest detail refresh failed: {message}",
                                )}
                            </p>
                        }
                    })
            }} {section.render(detail)} <div class="flex justify-start">
                <button
                    class="rounded-md border border-line bg-surface px-3 py-2 font-bold text-accent-strong hover:border-accent disabled:cursor-wait disabled:opacity-60"
                    type="button"
                    disabled={move || signals.detail_loading.get()}
                    on:click={move |_| refresh_detail()}
                >
                    {move || {
                        if signals.detail_loading.get() {
                            "Refreshing…"
                        } else {
                            "Refresh"
                        }
                    }}
                </button>
            </div>
        </div>
    }
    .into_view()
}

impl RuntimeSection {
    fn render(self, detail: &ApplicationDetailView) -> View {
        let app = detail.application.application.clone();
        let status = detail.status.clone();
        let observed = &detail.observed;
        let intent_generation = detail.application.generation;
        let resolved_generation = detail
            .application
            .resolved_generation
            .map_or_else(|| "none".to_owned(), |value| value.to_string());
        let runtime_health = status
            .runtime_health
            .clone()
            .unwrap_or_else(|| "unknown".to_owned());
        let application_id = app.id().to_string();
        let health = ApplicationHealth::from_server_state(status.state);
        match self {
            RuntimeSection::Overview => view! {
                <dl class="host-settings">
                    <dt>"Application ID"</dt>
                    <dd>
                        <code>{application_id}</code>
                    </dd>
                    <dt>"State"</dt>
                    <dd>
                        <span class={health_class(health)}>{status.state.to_string()}</span>
                    </dd>
                    <dt>"Generation"</dt>
                    <dd>{intent_generation}</dd>
                    <dt>"Resolved generation"</dt>
                    <dd>{resolved_generation}</dd>
                    <dt>"Runtime health"</dt>
                    <dd>{runtime_health}</dd>
                    <dt>"Networks / volumes"</dt>
                    <dd>
                        {format!("{} / {}", observed.network_count, observed.volume_count)}
                    </dd>
                </dl>
            }
            .into_view(),
            RuntimeSection::Services => view! {
                <section aria-labelledby="observed-title">
                    <h3 id="observed-title" class="mb-2 text-lg font-bold">
                        "Observed services"
                    </h3>
                    {if observed.services.is_empty() {
                        view! { <p class="m-0 text-muted">"No observed services."</p> }
                            .into_view()
                    } else {
                        view! {
                            <ul class="grid gap-2">
                                {observed
                                    .services
                                    .iter()
                                    .map(observed_service_view)
                                    .collect_view()}
                            </ul>
                        }
                            .into_view()
                    }}
                </section>
            }
            .into_view(),
            RuntimeSection::Diagnostics => {
                if detail.diagnostics.is_empty() {
                    view! { <p class="m-0 text-muted">"No diagnostics reported."</p> }.into_view()
                } else {
                    view! {
                        <ul class="grid gap-2">
                            {detail.diagnostics.iter().map(diagnostic_view).collect_view()}
                        </ul>
                    }
                    .into_view()
                }
            }
        }
    }
}

fn observed_service_view(service: &ObservedServiceView) -> View {
    let health = ApplicationHealth::from_convergence(&service.convergence);
    let image = service
        .image
        .clone()
        .unwrap_or_else(|| "Service not observed".into());
    let diagnostics = service
        .diagnostics
        .iter()
        .map(diagnostic_view)
        .collect_view();
    view! {
        <li class="grid gap-3 rounded-lg border border-line p-3 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-center">
            <div>
                <strong class="block break-words">{service.name.clone()}</strong>
                <span class="break-words text-muted">{image}</span>
            </div>
            <div class="flex flex-wrap items-center justify-start gap-2 text-sm text-muted sm:justify-end">
                <span class={health_class(health)}>{health.label()}</span>
                <span>
                    {format!("{} / {} healthy", service.healthy_replicas, service.desired_replicas)}
                </span>
            </div>
            {(!service.diagnostics.is_empty())
                .then(|| view! { <ul class="col-span-full grid gap-2">{diagnostics}</ul> })}
        </li>
    }
    .into_view()
}

fn diagnostic_view(diagnostic: &DiagnosticView) -> View {
    view! {
        <li class="flex gap-3 rounded-md bg-surface-muted p-2">
            <strong class="text-xs text-bad">{diagnostic.code.clone()}</strong>
            <span class="break-words">{diagnostic.message.clone()}</span>
        </li>
    }
    .into_view()
}
