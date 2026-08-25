//! Leptos client-side-rendered dashboard routes and shared data services.

use crate::state::{
    ApplicationHealth, ConnectionState, DataState, MAX_PAGES, PAGE_LIMIT, PaginationState,
    PollController,
};
use futures_util::StreamExt;
use gloo_timers::future::TimeoutFuture;
use leptos::{
    CollectView, DynAttrs, IntoView, RwSignal, SignalGet, SignalGetUntracked, SignalSet,
    SignalWith, View, component, create_effect, create_rw_signal, ev, mount_to_body, on_cleanup,
    provide_context, spawn_local, view, window_event_listener,
};
use leptos_router::{A, Outlet, Redirect, Route, Router, Routes, TrailingSlash, use_params_map};
use piqueld_client::{
    ApplicationDetailView, ApplicationStatusView, ApplicationView, Client, ClientError,
    DiagnosticView, ListApplicationsOptions, ObservedServiceView, Page, Source, SystemStatus,
};
use std::{cell::RefCell, rc::Rc};
use web_sys::window as browser_window;

#[derive(Clone, Debug)]
struct ApplicationRow {
    application: ApplicationView,
    status: Option<ApplicationStatusView>,
    status_error: Option<String>,
}

#[derive(Clone, Debug)]
struct DashboardSnapshot {
    system: SystemStatus,
    applications: Vec<ApplicationRow>,
    incomplete: bool,
}

#[derive(Clone, Debug)]
struct LoadFailure {
    unreachable: bool,
    message: String,
}

#[derive(Clone, Copy)]
struct DashboardSignals {
    system: RwSignal<Option<SystemStatus>>,
    applications: RwSignal<Vec<ApplicationRow>>,
    connection: RwSignal<ConnectionState>,
    data_state: RwSignal<DataState>,
    refresh_error: RwSignal<Option<String>>,
    refreshing: RwSignal<bool>,
    pagination_incomplete: RwSignal<bool>,
    selected_id: RwSignal<Option<String>>,
    detail: RwSignal<Option<ApplicationDetailView>>,
    detail_loading: RwSignal<bool>,
    detail_request: RwSignal<u64>,
    detail_error: RwSignal<Option<String>>,
}

impl DashboardSignals {
    fn new() -> Self {
        Self {
            system: create_rw_signal(None),
            applications: create_rw_signal(Vec::new()),
            connection: create_rw_signal(ConnectionState::Loading),
            data_state: create_rw_signal(DataState::Loading),
            refresh_error: create_rw_signal(None),
            refreshing: create_rw_signal(false),
            pagination_incomplete: create_rw_signal(false),
            selected_id: create_rw_signal(None),
            detail: create_rw_signal(None),
            detail_loading: create_rw_signal(false),
            detail_request: create_rw_signal(0),
            detail_error: create_rw_signal(None),
        }
    }
}

type Refresh = Rc<dyn Fn()>;

#[derive(Clone)]
struct DashboardContext {
    signals: DashboardSignals,
    client: Client,
    refresh: Refresh,
}

/// Mounts the CSR application into the document body.
pub fn mount() {
    mount_to_body(|| view! { <App/> });
}

#[component]
fn App() -> impl IntoView {
    view! {
        <Router trailing_slash=TrailingSlash::Exact fallback=|| view! { <NotFoundPage/> }>
            <Routes>
                <Route path="/" view=DashboardLayout>
                    <Route path="/" view=OverviewPage/>
                    <Route path="/applications" view=ApplicationsPage/>
                    <Route path="/applications/:id" view=ApplicationDetailPage/>
                </Route>
                <Route path="/dashboard" view=DashboardLayout>
                    <Route path="" view=DashboardRedirect/>
                    <Route path="/" view=OverviewPage/>
                    <Route path="/applications" view=ApplicationsPage/>
                    <Route path="/applications/:id" view=ApplicationDetailPage/>
                    <Route path="/*any" view=NotFoundPage/>
                </Route>
                <Route path="/*any" view=NotFoundPage/>
            </Routes>
        </Router>
    }
}

#[component]
fn DashboardRedirect() -> impl IntoView {
    view! { <Redirect path="/dashboard/"/> }
}

#[component]
fn DashboardLayout() -> impl IntoView {
    let signals = DashboardSignals::new();
    let client = Client::browser();
    let controller = Rc::new(RefCell::new(PollController::new()));
    controller.borrow_mut().set_hidden(document_hidden());

    let refresh: Refresh = {
        let client = client.clone();
        let controller = Rc::clone(&controller);
        Rc::new(move || {
            start_refresh(client.clone(), signals, Rc::clone(&controller), true);
        })
    };
    let context = DashboardContext {
        signals,
        client: client.clone(),
        refresh: Rc::clone(&refresh),
    };
    provide_context(context.clone());

    let visibility_listener = {
        let controller = Rc::clone(&controller);
        window_event_listener(ev::visibilitychange, move |_| {
            controller.borrow_mut().set_hidden(document_hidden());
        })
    };
    on_cleanup(move || visibility_listener.remove());

    start_refresh(client.clone(), signals, Rc::clone(&controller), true);
    spawn_poll_loop(client, signals, controller);

    view! {
        <a class="skip-link" href="#dashboard-main">"Skip to main content"</a>
        {dashboard_header(&context)}

        <main id="dashboard-main" class="mx-auto w-[calc(100%-2rem)] max-w-[1180px] pb-8" tabindex="-1">
            <section class="mb-4 rounded-xl border border-line border-l-4 border-l-accent bg-surface p-4 shadow-panel" aria-labelledby="read-only-title">
                <h2 id="read-only-title" class="mb-1 text-lg font-bold">"Read-only view"</h2>
                <p class="mb-0">"This dashboard shows daemon and application state. Use "<code>"piquelctl"</code>" for plan, apply, reconcile, and delete operations."</p>
            </section>

            {system_summary(signals)}
            {refresh_error(&context)}
            {stale_notice(signals)}

            <Outlet/>
        </main>
        <footer class="mx-auto w-[calc(100%-2rem)] max-w-[1180px] pb-8 text-sm text-muted">"piqueld · loopback dashboard · API v1"</footer>
    }
}

fn dashboard_context() -> DashboardContext {
    leptos::use_context().expect("dashboard routes are descendants of DashboardLayout")
}

fn dashboard_header(context: &DashboardContext) -> View {
    let signals = context.signals;
    let refresh = Rc::clone(&context.refresh);
    view! {
        <header class="site-header mx-auto flex w-[calc(100%-2rem)] max-w-[1180px] flex-col items-start justify-between gap-4 py-5 sm:flex-row sm:items-end sm:py-8">
            <div>
                <p class="mb-1 text-xs font-extrabold tracking-[.12em] text-accent">"PIQUELD CONTROL PLANE"</p>
                <h1 class="mb-0 text-4xl font-extrabold tracking-[-.04em] sm:text-5xl">"Dashboard"</h1>
                <nav class="mt-3 flex flex-wrap gap-2" aria-label="Dashboard navigation">
                    <A class="rounded-md px-2 py-1 text-sm font-bold text-accent hover:bg-surface-muted" href="/dashboard/">"Overview"</A>
                    <A class="rounded-md px-2 py-1 text-sm font-bold text-accent hover:bg-surface-muted" href="/dashboard/applications">"Applications"</A>
                </nav>
            </div>
            <div class="header-actions flex flex-wrap items-center justify-start gap-3 sm:justify-end">
                <p class="m-0 flex items-center gap-2 font-bold text-muted" role="status" aria-live="polite">
                    <span class=move || status_dot_class(signals.connection.get()) aria-hidden="true"></span>
                    "Daemon: " {move || connection_label(signals.connection.get())}
                </p>
                <button
                    class="rounded-md border border-line bg-surface px-3 py-2 font-bold text-accent-strong hover:border-accent disabled:cursor-wait disabled:opacity-60"
                    type="button"
                    disabled=move || signals.refreshing.get()
                    on:click=move |_| refresh()
                >
                    {move || if signals.refreshing.get() { "Refreshing…" } else { "Refresh" }}
                </button>
            </div>
        </header>
    }
    .into_view()
}

#[component]
fn OverviewPage() -> impl IntoView {
    let context = dashboard_context();
    let signals = context.signals;
    view! {
        <section class="space-y-4" aria-labelledby="overview-title">
            <div class="flex items-start justify-between gap-3">
                <div>
                    <p class="mb-1 text-xs font-extrabold tracking-[.12em] text-accent">"AT A GLANCE"</p>
                    <h2 id="overview-title" class="mb-0 text-2xl font-bold">"Overview"</h2>
                </div>
                <A class="rounded-md border border-line bg-surface px-3 py-2 font-bold text-accent-strong hover:border-accent" href="/dashboard/applications">"View applications"</A>
            </div>
            <div class="grid gap-4 sm:grid-cols-3">
                <div class="rounded-xl border border-line bg-surface p-4 shadow-panel">
                    <p class="mb-1 text-xs font-extrabold tracking-[.12em] text-accent">"APPLICATIONS"</p>
                    <p class="text-3xl font-extrabold" aria-live="polite">{move || signals.applications.get().len()}</p>
                    <p class="mb-0 text-sm text-muted">"Desired applications"</p>
                </div>
                <div class="rounded-xl border border-line bg-surface p-4 shadow-panel">
                    <p class="mb-1 text-xs font-extrabold tracking-[.12em] text-accent">"CONNECTION"</p>
                    <p class="text-3xl font-extrabold">{move || connection_label(signals.connection.get())}</p>
                    <p class="mb-0 text-sm text-muted">"Latest API refresh"</p>
                </div>
                <div class="rounded-xl border border-line bg-surface p-4 shadow-panel">
                    <p class="mb-1 text-xs font-extrabold tracking-[.12em] text-accent">"DATA"</p>
                    <p class="text-3xl font-extrabold">{move || data_state_label(signals.data_state.get())}</p>
                    <p class="mb-0 text-sm text-muted">"Current dashboard view"</p>
                </div>
            </div>
            {compact_applications(signals)}
        </section>
    }
}

#[component]
fn ApplicationsPage() -> impl IntoView {
    let context = dashboard_context();
    applications_panel(context.signals)
}

#[component]
fn ApplicationDetailPage() -> impl IntoView {
    let context = dashboard_context();
    let signals = context.signals;
    let params = use_params_map();
    let client = context.client.clone();

    create_effect(move |_| {
        let id = params.with(|params| params.get("id").cloned());
        let Some(id) = id else {
            return;
        };
        if signals.selected_id.get_untracked().as_deref() == Some(id.as_str())
            && signals.detail.get_untracked().is_some()
        {
            return;
        }
        signals.selected_id.set(Some(id.clone()));
        signals.detail.set(None);
        load_detail(client.clone(), signals, id);
    });

    view! {
        <section class="rounded-xl border border-line bg-surface p-5 shadow-panel" aria-labelledby="detail-title">
            <div class="mb-4 flex items-start justify-between gap-3">
                <div>
                    <p class="mb-1 text-xs font-extrabold tracking-[.12em] text-accent">"OBSERVED STATE"</p>
                    <h2 id="detail-title" class="mb-0 text-2xl font-bold">"Application detail"</h2>
                </div>
                <A class="rounded-md px-2 py-1 text-sm font-bold text-accent hover:bg-surface-muted" href="/dashboard/applications">"Back to applications"</A>
            </div>
            {move || {
                let detail = signals.detail.get();
                if signals.detail_loading.get() && detail.is_none() {
                    view! { <p class="rounded-lg border border-line bg-surface-muted p-5" role="status">"Loading application detail…"</p> }.into_view()
                } else if let Some(detail) = detail {
                    detail_view(&detail, signals, context.client.clone())
                } else {
                    let message = signals.detail_error.get().unwrap_or_else(|| "Application detail is unavailable.".into());
                    view! { <div class="rounded-lg border border-line bg-surface-muted p-5" role="alert"><h3 class="mb-1 text-lg font-bold">"Detail unavailable"</h3><p class="mb-0">{message}</p></div> }.into_view()
                }
            }}
        </section>
    }
}

#[component]
fn NotFoundPage() -> impl IntoView {
    view! {
        <section class="rounded-xl border border-line bg-surface p-6 shadow-panel" aria-labelledby="not-found-title">
            <p class="mb-1 text-xs font-extrabold tracking-[.12em] text-accent">"NOT FOUND"</p>
            <h2 id="not-found-title" class="mb-2 text-2xl font-bold">"Dashboard page not found"</h2>
            <p class="mb-4 text-muted">"Choose a known dashboard destination to continue."</p>
            <A class="rounded-md border border-line bg-surface px-3 py-2 font-bold text-accent-strong hover:border-accent" href="/dashboard/">"Return to overview"</A>
        </section>
    }
}

fn system_summary(signals: DashboardSignals) -> View {
    view! {
        {move || signals.system.get().map(|system| view! {
            <section class="mb-4 flex flex-col justify-between gap-4 rounded-xl border border-line bg-surface p-4 shadow-panel sm:flex-row sm:items-center" aria-labelledby="system-title">
                <div>
                    <p class="mb-1 text-xs font-extrabold tracking-[.12em] text-accent">"DAEMON"</p>
                    <h2 id="system-title" class="mb-0 text-xl font-bold">"Available"</h2>
                </div>
                <dl class="grid w-full gap-3 sm:w-auto sm:grid-cols-3">
                    <div><dt class="text-xs font-extrabold uppercase tracking-[.05em] text-muted">"Version"</dt><dd class="mt-1">{system.daemon_version}</dd></div>
                    <div><dt class="text-xs font-extrabold uppercase tracking-[.05em] text-muted">"API"</dt><dd class="mt-1">{system.api_version}</dd></div>
                    <div><dt class="text-xs font-extrabold uppercase tracking-[.05em] text-muted">"Instance"</dt><dd class="mt-1"><code>{short_id(&system.instance_id)}</code></dd></div>
                </dl>
            </section>
        })}
    }
    .into_view()
}

fn refresh_error(context: &DashboardContext) -> View {
    let signals = context.signals;
    let refresh = Rc::clone(&context.refresh);
    view! {
        {move || signals.refresh_error.get().map(|message| {
            let retry = Rc::clone(&refresh);
            view! {
                <div class="mb-4 rounded-xl border border-line border-l-4 border-l-bad bg-surface p-4 shadow-panel" role="alert" aria-live="assertive">
                    <h2 class="mb-1 text-lg font-bold">{if signals.connection.get() == ConnectionState::Unreachable { "Daemon unreachable" } else { "Refresh failed" }}</h2>
                    <p class="mb-2">{message}</p>
                    <button class="rounded-md border border-line bg-surface px-3 py-2 font-bold text-accent-strong hover:border-accent" type="button" on:click=move |_| retry()>"Try again"</button>
                </div>
            }
        })}
    }
    .into_view()
}

fn stale_notice(signals: DashboardSignals) -> View {
    view! {
        {move || (signals.data_state.get() == DataState::Stale).then(|| view! {
            <p class="mb-4 rounded-lg border border-warn bg-warn-bg p-3 text-warn" role="status">"Showing the last successful view; the latest refresh failed."</p>
        })}
    }
    .into_view()
}

fn compact_applications(signals: DashboardSignals) -> View {
    view! {
        <section class="rounded-xl border border-line bg-surface p-5 shadow-panel" aria-labelledby="summary-applications-title">
            <div class="mb-4 flex items-start justify-between gap-3">
                <div>
                    <p class="mb-1 text-xs font-extrabold tracking-[.12em] text-accent">"DESIRED STATE"</p>
                    <h2 id="summary-applications-title" class="mb-0 text-xl font-bold">"Applications"</h2>
                </div>
                <A class="rounded-md px-2 py-1 text-sm font-bold text-accent hover:bg-surface-muted" href="/dashboard/applications">"See all"</A>
            </div>
            {move || match signals.data_state.get() {
                DataState::Loading => view! { <p class="rounded-lg border border-line bg-surface-muted p-5" role="status">"Loading applications…"</p> }.into_view(),
                DataState::Empty => view! { <p class="rounded-lg border border-line bg-surface-muted p-5">"No applications are configured yet."</p> }.into_view(),
                DataState::Ready | DataState::Stale => view! {
                    <ul class="grid gap-3 sm:grid-cols-2" aria-label="Application summary">
                        {move || signals.applications.get().into_iter().take(4).map(compact_application_card).collect_view()}
                    </ul>
                }.into_view(),
            }}
        </section>
    }
    .into_view()
}

fn compact_application_card(row: ApplicationRow) -> View {
    let id = row.application.application.id.to_string();
    let name = row.application.application.metadata.name.clone();
    let health = row_health(&row);
    view! {
        <li class="rounded-lg border border-line bg-surface-muted p-3">
            <div class="flex items-start justify-between gap-3">
                <div>
                    <h3 class="mb-1 font-bold">{name}</h3>
                    <p class="mb-0 text-sm text-muted"><code>{short_id(&id)}</code></p>
                </div>
                <span class=health_class(health)>{health.label()}</span>
            </div>
        </li>
    }
    .into_view()
}

fn applications_panel(signals: DashboardSignals) -> View {
    view! {
        <section class="rounded-xl border border-line bg-surface p-5 shadow-panel" aria-labelledby="applications-title">
            <div class="mb-4 flex items-start justify-between gap-3">
                <div>
                    <p class="mb-1 text-xs font-extrabold tracking-[.12em] text-accent">"DESIRED STATE"</p>
                    <h2 id="applications-title" class="mb-0 text-2xl font-bold">"Applications"</h2>
                </div>
                <p class="m-0 text-sm text-muted">{move || format!("{} shown", signals.applications.get().len())}</p>
            </div>
            {move || signals.pagination_incomplete.get().then(|| view! {
                <p class="mb-4 rounded-lg border border-warn bg-warn-bg p-3 text-warn" role="status">"Application list incomplete: the dashboard stopped at a safe pagination bound or repeated cursor. Use piquelctl for the complete list."</p>
            })}
            {move || match signals.data_state.get() {
                DataState::Loading => view! { <p class="rounded-lg border border-line bg-surface-muted p-5" role="status">"Loading applications…"</p> }.into_view(),
                DataState::Empty => view! { <div class="rounded-lg border border-line bg-surface-muted p-5"><h3 class="mb-1 text-lg font-bold">"No applications"</h3><p class="mb-0">"The daemon has no desired applications yet. Use piquelctl to create one."</p></div> }.into_view(),
                DataState::Ready | DataState::Stale => ().into_view(),
            }}
            <ul class="application-list grid gap-3 sm:grid-cols-2 lg:grid-cols-3" aria-label="Application list">
                {move || signals.applications.get().into_iter().map(|row| application_card(&row, signals)).collect_view()}
            </ul>
        </section>
    }
    .into_view()
}

fn application_card(row: &ApplicationRow, signals: DashboardSignals) -> View {
    let id = row.application.application.id.to_string();
    let id_for_class = id.clone();
    let selected_id = signals.selected_id;
    let name = row.application.application.metadata.name.clone();
    let name_for_label = name.clone();
    let health = row_health(row);
    let status_text = row_status_text(row);
    let desired_replicas = desired_replicas(&row.application);
    let href = format!("/dashboard/applications/{id}");
    view! {
        <li>
            <article class="application-card flex h-full flex-col gap-2 rounded-lg border border-line bg-surface-muted p-4" class:selected=move || selected_id.get().as_deref() == Some(id_for_class.as_str())>
                <div class="flex items-start justify-between gap-3">
                    <span class=health_class(health)>{health.label()}</span>
                    <span class="text-xs text-muted">{format!("Generation {}", row.application.generation)}</span>
                </div>
                <h3 class="mb-0 text-lg font-bold">{name}</h3>
                <p class="mb-0 text-sm text-muted"><code>{short_id(&id)}</code></p>
                <dl class="my-1 grid grid-cols-2 gap-3">
                    <div><dt class="text-xs font-extrabold uppercase tracking-[.05em] text-muted">"Desired replicas"</dt><dd class="mt-1">{desired_replicas}</dd></div>
                    <div><dt class="text-xs font-extrabold uppercase tracking-[.05em] text-muted">"Observed state"</dt><dd class="mt-1">{status_text}</dd></div>
                </dl>
                <A class="mt-auto block w-full rounded-md border border-line bg-surface px-3 py-2 text-center font-bold text-accent-strong hover:border-accent" href=href attr:aria-label=format!("View details for {name_for_label}")>"View details"</A>
            </article>
        </li>
    }
    .into_view()
}

fn detail_view(detail: &ApplicationDetailView, signals: DashboardSignals, client: Client) -> View {
    let app = detail.application.application.clone();
    let status = detail.status.clone();
    let operation = detail.latest_operation.clone();
    let diagnostics = detail.diagnostics.clone();
    let observed = detail.observed.clone();
    let application_name = app.metadata.name.clone();
    let application_id = app.id.to_string();
    let desired_services = app
        .spec
        .services
        .iter()
        .map(|service| {
            let name = service.name.clone();
            let image = match &service.source {
                Source::Image { image } => image.clone(),
            };
            let replicas = service.replicas;
            view! {
                <li class="grid gap-3 rounded-lg border border-line p-3 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-center">
                    <div><strong class="block break-words">{name}</strong><span class="break-words text-muted">{image}</span></div>
                    <span class="rounded-full bg-surface-muted px-2 py-1 text-sm text-muted">{format!("{replicas} desired")}</span>
                </li>
            }
        })
        .collect_view();
    let observed_services = observed
        .services
        .iter()
        .map(observed_service_view)
        .collect_view();
    let diagnostics_view = diagnostics.iter().map(diagnostic_view).collect_view();
    let operation_view = operation.map(|operation| {
        let steps = operation
            .steps
            .iter()
            .map(|step| {
                let step_state = step.state.clone();
                view! { <li class="flex justify-between gap-3 rounded-md bg-surface-muted p-2"><span>{step.action.clone()}</span><span class="text-muted">{step_state}</span></li> }
            })
            .collect_view();
        view! {
            <section class="border-t border-line pt-4" aria-labelledby="operation-title">
                <h3 id="operation-title" class="mb-2 text-lg font-bold">"Latest operation"</h3>
                <p><strong>{operation.kind.clone()}</strong>" · "{operation.state.clone()}</p>
                <ul class="grid gap-2">{steps}</ul>
            </section>
        }
    });
    let health = ApplicationHealth::from_server_state(&status.state);
    let refresh_detail = {
        let id = application_id.clone();
        move || load_detail(client.clone(), signals, id.clone())
    };
    view! {
        <div class="grid gap-4">
            {move || signals.detail_error.get().map(|message| view! {
                <p class="rounded-lg border border-warn bg-warn-bg p-3 text-warn" role="status">
                    {format!("Showing the last successful detail; the latest detail refresh failed: {message}")}
                </p>
            })}
            <div class="detail-title-row flex items-start justify-between gap-3">
                <div><h3 class="mb-1 text-xl font-bold">{application_name}</h3><p class="mb-0 text-sm text-muted"><code>{application_id}</code></p></div>
                <span class=health_class(health)>{health.label()}</span>
            </div>
            <dl class="grid gap-3 rounded-lg bg-surface-muted p-3 sm:grid-cols-3">
                <div><dt class="text-xs font-extrabold uppercase tracking-[.05em] text-muted">"Desired generation"</dt><dd class="mt-1">{detail.application.generation}</dd></div>
                <div><dt class="text-xs font-extrabold uppercase tracking-[.05em] text-muted">"Observed generation"</dt><dd class="mt-1">{status.observed_generation.map_or_else(|| "Not observed".into(), |generation| generation.to_string())}</dd></div>
                <div><dt class="text-xs font-extrabold uppercase tracking-[.05em] text-muted">"Networks / volumes"</dt><dd class="mt-1">{format!("{} / {}", observed.network_count, observed.volume_count)}</dd></div>
            </dl>
            <p class="m-0 border-l-4 border-l-accent bg-surface-muted p-3"><span class=health_class(health)>{health.label()}</span> {status.message.clone().unwrap_or_else(|| "No additional daemon diagnostic.".into())}</p>
            <div class="flex justify-end"><button class="rounded-md border border-line bg-surface px-3 py-2 font-bold text-accent-strong hover:border-accent disabled:cursor-wait disabled:opacity-60" type="button" disabled=move || signals.detail_loading.get() on:click=move |_| refresh_detail()>{move || if signals.detail_loading.get() { "Refreshing…" } else { "Refresh detail" }}</button></div>
            <section class="border-t border-line pt-4" aria-labelledby="desired-title">
                <h3 id="desired-title" class="mb-2 text-lg font-bold">"Desired services"</h3>
                <ul class="grid gap-2">{desired_services}</ul>
            </section>
            <section class="border-t border-line pt-4" aria-labelledby="observed-title">
                <h3 id="observed-title" class="mb-2 text-lg font-bold">"Observed services"</h3>
                <ul class="grid gap-2">{observed_services}</ul>
            </section>
            <section class="border-t border-line pt-4" aria-labelledby="diagnostic-title">
                <h3 id="diagnostic-title" class="mb-2 text-lg font-bold">"Reconciliation diagnostics"</h3>
                {if diagnostics.is_empty() {
                    view! { <p class="m-0 text-muted">"No diagnostics reported."</p> }.into_view()
                } else {
                    view! { <ul class="grid gap-2">{diagnostics_view}</ul> }.into_view()
                }}
            </section>
            {operation_view}
        </div>
    }
    .into_view()
}

fn observed_service_view(service: &ObservedServiceView) -> View {
    let health = ApplicationHealth::from_server_state(&service.convergence);
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
            <div><strong class="block break-words">{service.name.clone()}</strong><span class="break-words text-muted">{image}</span></div>
            <div class="flex flex-wrap items-center justify-start gap-2 text-sm text-muted sm:justify-end"><span class=health_class(health)>{health.label()}</span><span>{format!("{} / {} healthy", service.healthy_replicas, service.desired_replicas)}</span></div>
            {(!service.diagnostics.is_empty()).then(|| view! { <ul class="col-span-full grid gap-2">{diagnostics}</ul> })}
        </li>
    }
    .into_view()
}

fn diagnostic_view(diagnostic: &DiagnosticView) -> View {
    view! { <li class="flex gap-3 rounded-md bg-surface-muted p-2"><strong class="text-xs text-bad">{diagnostic.code.clone()}</strong><span class="break-words">{diagnostic.message.clone()}</span></li> }.into_view()
}

fn start_refresh(
    client: Client,
    signals: DashboardSignals,
    controller: Rc<RefCell<PollController>>,
    manual: bool,
) {
    if manual {
        controller.borrow_mut().request_manual_refresh();
    }
    if !controller.borrow_mut().begin_request() {
        return;
    }
    signals.refreshing.set(true);
    spawn_local(async move {
        match fetch_snapshot(&client).await {
            Ok(snapshot) => {
                controller.borrow_mut().record_success();
                signals.system.set(Some(snapshot.system));
                signals.applications.set(snapshot.applications);
                signals.pagination_incomplete.set(snapshot.incomplete);
                signals.connection.set(ConnectionState::Reachable);
                signals
                    .data_state
                    .set(if signals.applications.get_untracked().is_empty() {
                        DataState::Empty
                    } else {
                        DataState::Ready
                    });
                signals.refresh_error.set(None);
                signals.refreshing.set(false);
                if let Some(id) = signals.selected_id.get_untracked() {
                    if signals
                        .applications
                        .get_untracked()
                        .iter()
                        .any(|row| row.application.application.id.to_string() == id)
                    {
                        load_detail(client.clone(), signals, id);
                    } else {
                        signals.selected_id.set(None);
                        signals.detail.set(None);
                    }
                }
            }
            Err(failure) => {
                controller.borrow_mut().record_failure();
                signals.connection.set(if failure.unreachable {
                    ConnectionState::Unreachable
                } else {
                    ConnectionState::Failed
                });
                if signals.data_state.get_untracked() != DataState::Loading {
                    signals.data_state.set(DataState::Stale);
                }
                signals.refresh_error.set(Some(failure.message));
                signals.refreshing.set(false);
            }
        }
    });
}

fn load_detail(client: Client, signals: DashboardSignals, id: String) {
    let request = signals.detail_request.get_untracked().wrapping_add(1);
    signals.detail_request.set(request);
    signals.detail_loading.set(true);
    signals.detail_error.set(None);
    spawn_local(async move {
        let result = client.application_detail(&id).await;
        if signals.detail_request.get_untracked() != request
            || signals.selected_id.get_untracked().as_deref() != Some(id.as_str())
        {
            return;
        }
        match result {
            Ok(detail) => signals.detail.set(Some(detail)),
            Err(error) => signals.detail_error.set(Some(client_error_message(&error))),
        }
        signals.detail_loading.set(false);
    });
}

fn spawn_poll_loop(
    client: Client,
    signals: DashboardSignals,
    controller: Rc<RefCell<PollController>>,
) {
    spawn_local(async move {
        loop {
            let delay = controller.borrow().delay();
            let milliseconds = u32::try_from(delay.as_millis()).unwrap_or(u32::MAX);
            TimeoutFuture::new(milliseconds).await;
            start_refresh(client.clone(), signals, Rc::clone(&controller), false);
        }
    });
}

async fn fetch_snapshot(client: &Client) -> Result<DashboardSnapshot, LoadFailure> {
    let system = client
        .system_status()
        .await
        .map_err(|error| load_failure(&error))?;
    let mut pagination = PaginationState::new();
    let mut cursor = None;
    let mut applications = Vec::new();
    loop {
        let page: Page<ApplicationView> = client
            .applications_with(&ListApplicationsOptions {
                cursor: cursor.clone(),
                limit: Some(PAGE_LIMIT),
            })
            .await
            .map_err(|error| load_failure(&error))?;
        let next_cursor = page.next_cursor.clone();
        // Status reads are independent, so a bounded pool keeps one slow
        // application from serializing the whole refresh.
        let statuses = futures_util::stream::iter(page.items.into_iter().map(|application| {
            let client = client.clone();
            async move {
                let id = application.application.id.to_string();
                let (status, status_error) = match client.application_status(&id).await {
                    Ok(status) => (Some(status), None),
                    Err(error) => (None, Some(client_error_message(&error))),
                };
                ApplicationRow {
                    application,
                    status,
                    status_error,
                }
            }
        }))
        .buffered(8)
        .collect::<Vec<_>>()
        .await;
        applications.extend(statuses);
        pagination.record_page(next_cursor);
        cursor = pagination.next_cursor().map(str::to_owned);
        if cursor.is_none() || pagination.pages_loaded() >= MAX_PAGES {
            break;
        }
    }
    Ok(DashboardSnapshot {
        system,
        applications,
        incomplete: pagination.incomplete(),
    })
}

fn load_failure(error: &ClientError) -> LoadFailure {
    LoadFailure {
        unreachable: matches!(error, ClientError::Transport { .. }),
        message: client_error_message(error),
    }
}

fn client_error_message(error: &ClientError) -> String {
    match error {
        ClientError::Endpoint { message } => {
            format!("The dashboard endpoint is invalid: {message}")
        }
        ClientError::Transport { message } => format!("Could not reach piqueld: {message}"),
        ClientError::Api { error, .. } => error.message.clone(),
        ClientError::Decode => "The daemon returned an invalid public API response.".into(),
    }
}

fn document_hidden() -> bool {
    browser_window()
        .and_then(|window| window.document())
        .is_some_and(|document| document.hidden())
}

fn connection_label(state: ConnectionState) -> &'static str {
    match state {
        ConnectionState::Loading => "Checking…",
        ConnectionState::Reachable => "Reachable",
        ConnectionState::Failed => "Request failed",
        ConnectionState::Unreachable => "Unreachable",
    }
}

fn data_state_label(state: DataState) -> &'static str {
    match state {
        DataState::Loading => "Loading",
        DataState::Ready => "Ready",
        DataState::Empty => "Empty",
        DataState::Stale => "Stale",
    }
}

fn status_dot_class(state: ConnectionState) -> &'static str {
    match state {
        ConnectionState::Reachable => "inline-block h-3 w-3 rounded-full bg-ok",
        ConnectionState::Failed | ConnectionState::Unreachable => {
            "inline-block h-3 w-3 rounded-full bg-bad"
        }
        ConnectionState::Loading => "inline-block h-3 w-3 rounded-full bg-pending",
    }
}

fn row_health(row: &ApplicationRow) -> ApplicationHealth {
    row.status_error.as_ref().map_or_else(
        || {
            row.status
                .as_ref()
                .map_or(ApplicationHealth::Pending, |status| {
                    ApplicationHealth::from_server_state(&status.state)
                })
        },
        |_| ApplicationHealth::Failed,
    )
}

fn row_status_text(row: &ApplicationRow) -> String {
    row.status_error.clone().unwrap_or_else(|| {
        row.status
            .as_ref()
            .map_or_else(|| "Not observed".into(), |status| status.state.clone())
    })
}

fn desired_replicas(application: &ApplicationView) -> u32 {
    application
        .application
        .spec
        .services
        .iter()
        .map(|service| u32::from(service.replicas))
        .sum()
}

fn health_class(health: ApplicationHealth) -> &'static str {
    match health {
        ApplicationHealth::Converged => {
            "inline-flex w-fit items-center rounded-full bg-ok-bg px-2 py-1 text-xs font-extrabold text-ok"
        }
        ApplicationHealth::Degraded => {
            "inline-flex w-fit items-center rounded-full bg-warn-bg px-2 py-1 text-xs font-extrabold text-warn"
        }
        ApplicationHealth::Failed => {
            "inline-flex w-fit items-center rounded-full bg-bad-bg px-2 py-1 text-xs font-extrabold text-bad"
        }
        ApplicationHealth::Pending => {
            "inline-flex w-fit items-center rounded-full bg-pending-bg px-2 py-1 text-xs font-extrabold text-pending"
        }
    }
}

fn short_id(value: &str) -> String {
    value.chars().take(12).collect()
}
