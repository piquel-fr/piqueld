//! Leptos client-side-rendered dashboard components.

use crate::state::{
    API_PREFIX, ApplicationHealth, ConflictState, ConnectionState, DataState, FieldErrors,
    LogBuffer, MAX_LOG_LINES, MAX_PAGES, PAGE_LIMIT, PaginationState, PollController,
    ReconnectState, SecretDraft, blank_manifest, blank_service, set_source_kind,
};
use gloo_timers::future::TimeoutFuture;
use leptos::{
    Callback, CollectView, IntoView, RwSignal, SignalGet, SignalGetUntracked, SignalSet, View,
    component, create_rw_signal, ev, mount_to_body, on_cleanup, spawn_local, view,
    window_event_listener,
};
use piqueld_client::{
    ApplicationDetailView, ApplicationLogsOptions, ApplicationStatusView, ApplicationView,
    BuildView, Client, ClientError, ContainerLogView, CreateApplicationRequest,
    DeleteApplicationRequest, DiagnosticView, ListApplicationsOptions, ListSecretsOptions,
    MAX_STATE_ARCHIVE_BYTES, ObservedServiceView, Page, PlanApplicationRequest, PlanView,
    ReplaceApplicationRequest, ReplacePlanRequest, SecretMetadata, ServiceStatusView, Source,
    StateExportMode, StateImportResult, SystemStatus,
};
use piqueld_core::manifest::{
    ApplicationManifest, ApplicationSpecInput, HealthCheck, HealthCheckInput, MetadataInput,
    MountInput, NormalizedApplication, ResourceLimitsInput, RouteInput, SecretReferenceInput,
    ServiceInput, SourceInput, VolumeInput,
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use wasm_bindgen::{JsCast, closure::Closure};
use wasm_bindgen_futures::spawn_local;
use web_sys::window as browser_window;
use web_sys::{EventSource, HtmlInputElement, MessageEvent, Url};
use zeroize::Zeroize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Screen {
    Applications,
    Editor,
    Secrets,
    State,
}

#[derive(Clone, Debug, Default)]
struct Notice {
    message: String,
    error: bool,
}

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

/// Mounts the CSR application into the document body.
pub fn mount() {
    mount_to_body(|| view! { <App/> });
}

type Refresh = Rc<dyn Fn()>;
type SelectApplication = Rc<dyn Fn(String)>;

#[component]
fn App() -> impl IntoView {
    let screen = create_rw_signal(Screen::Applications);
    let editor_id = create_rw_signal(None::<String>);
    let open_new = {
        Callback::new(move |()| {
            editor_id.set(None);
            screen.set(Screen::Editor);
        })
    };
    let open_editor = {
        Callback::new(move |id: String| {
            editor_id.set(Some(id));
            screen.set(Screen::Editor);
        })
    };
    let close_editor = { Callback::new(move |()| screen.set(Screen::Applications)) };
    view! {
        <a class="skip-link" href="#dashboard-content">"Skip to content"</a>
        <header class="topbar">
            <a class="brand" href="/" on:click=move |event| { event.prevent_default(); screen.set(Screen::Applications); }>
                <span aria-hidden="true">"◆"</span> " piqueld"
            </a>
            <nav aria-label="Primary">
                <button class:active=move || screen.get() == Screen::Applications on:click=move |_| screen.set(Screen::Applications)>"Applications"</button>
                <button class:active=move || screen.get() == Screen::Secrets on:click=move |_| screen.set(Screen::Secrets)>"Secrets"</button>
                <button class:active=move || screen.get() == Screen::State on:click=move |_| screen.set(Screen::State)>"State"</button>
            </nav>
        </header>
        <main id="dashboard-content" tabindex="-1">
            {move || match screen.get() {
                Screen::Applications => view! { <Dashboard on_new=open_new on_manage=open_editor/> }.into_view(),
                Screen::Editor => view! { <ApplicationEditor application_id=editor_id.get() on_done=close_editor/> }.into_view(),
                Screen::Secrets => view! { <Secrets/> }.into_view(),
                Screen::State => view! { <StateTransfer/> }.into_view(),
            }}
        </main>
        <footer class="site-footer">"piqueld · API v1"</footer>
    }
}

#[component]
#[allow(clippy::too_many_lines)]
fn Dashboard(on_new: Callback<()>, on_manage: Callback<String>) -> impl IntoView {
    let signals = DashboardSignals {
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
    };
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

    let select: SelectApplication = {
        let client = client.clone();
        Rc::new(move |id: String| {
            signals.selected_id.set(Some(id.clone()));
            signals.detail.set(None);
            load_detail(client.clone(), signals, id);
        })
    };

    let visibility_listener = {
        let controller = Rc::clone(&controller);
        window_event_listener(ev::visibilitychange, move |_| {
            controller.borrow_mut().set_hidden(document_hidden());
        })
    };
    on_cleanup(move || visibility_listener.remove());

    start_refresh(client.clone(), signals, Rc::clone(&controller), true);
    spawn_poll_loop(client, signals, controller);

    dashboard_view(signals, &refresh, select, on_new, on_manage)
}

fn dashboard_view(
    signals: DashboardSignals,
    refresh: &Refresh,
    select: SelectApplication,
    on_new: Callback<()>,
    on_manage: Callback<String>,
) -> View {
    view! {
        <a class="skip-link" href="#dashboard-main">"Skip to main content"</a>
        {dashboard_header(signals, Rc::clone(refresh), on_new)}

        <main id="dashboard-main" tabindex="-1">
            <section class="notice notice-info" aria-labelledby="read-only-title">
                <h2 id="read-only-title">"Read-only view"</h2>
                <p>"This dashboard shows daemon and application state. Use "<code>"piquelctl"</code>" for plan, apply, reconcile, and delete operations."</p>
            </section>

            {system_summary(signals)}
            {refresh_error(signals, Rc::clone(refresh))}
            {stale_notice(signals)}

            <div class="dashboard-grid">
                {applications_panel(signals, select)}

                <ApplicationDetail signals=signals on_manage=on_manage/>
            </div>
        </main>
        <footer class="site-footer">"piqueld · loopback dashboard · API v1"</footer>
    }
    .into_view()
}

fn dashboard_header(signals: DashboardSignals, refresh: Refresh, on_new: Callback<()>) -> View {
    view! {
        <header class="site-header">
            <div>
                <p class="eyebrow">"PIQUELD CONTROL PLANE"</p>
                <h1>"Dashboard"</h1>
            </div>
            <div class="header-actions">
                <p class="daemon-status" role="status" aria-live="polite">
                    <span class:status-ok=move || signals.connection.get() == ConnectionState::Reachable class:status-bad=move || matches!(signals.connection.get(), ConnectionState::Failed | ConnectionState::Unreachable) class="status-dot" aria-hidden="true"></span>
                    "Daemon: " {move || connection_label(signals.connection.get())}
                </p>
                <button
                    class="button button-secondary"
                    type="button"
                    disabled=move || signals.refreshing.get()
                    on:click=move |_| refresh()
                >
                    {move || if signals.refreshing.get() { "Refreshing…" } else { "Refresh" }}
                </button>
                <button class="button button-primary" type="button" on:click=move |_| on_new.call(())>
                    "New application"
                </button>
            </div>
        </header>
    }
    .into_view()
}

fn system_summary(signals: DashboardSignals) -> View {
    view! {
        {move || signals.system.get().map(|system| view! {
            <section class="system-summary" aria-labelledby="system-title">
                <div>
                    <p class="eyebrow">"DAEMON"</p>
                    <h2 id="system-title">"Available"</h2>
                </div>
                <dl class="summary-list">
                    <div><dt>"Version"</dt><dd>{system.daemon_version}</dd></div>
                    <div><dt>"API"</dt><dd>{system.api_version}</dd></div>
                    <div><dt>"Instance"</dt><dd><code>{short_id(&system.instance_id)}</code></dd></div>
                </dl>
            </section>
        })}
    }
    .into_view()
}

fn refresh_error(signals: DashboardSignals, refresh: Refresh) -> View {
    view! {
        {move || signals.refresh_error.get().map(|message| {
            let retry = Rc::clone(&refresh);
            view! {
                <div class="notice notice-error" role="alert" aria-live="assertive">
                    <h2>{if signals.connection.get() == ConnectionState::Unreachable { "Daemon unreachable" } else { "Refresh failed" }}</h2>
                    <p>{message}</p>
                    <button class="button button-secondary" type="button" on:click=move |_| retry()>{"Try again"}</button>
                </div>
            }
        })}
    }
    .into_view()
}

fn stale_notice(signals: DashboardSignals) -> View {
    view! {
        {move || (signals.data_state.get() == DataState::Stale).then(|| view! {
            <p class="stale-banner" role="status">"Showing the last successful view; the latest refresh failed."</p>
        })}
    }
    .into_view()
}

fn applications_panel(signals: DashboardSignals, select: SelectApplication) -> View {
    view! {
        <section class="applications-panel" aria-labelledby="applications-title">
            <div class="section-heading">
                <div>
                    <p class="eyebrow">"DESIRED STATE"</p>
                    <h2 id="applications-title">"Applications"</h2>
                </div>
                <p class="muted">{move || format!("{} shown", signals.applications.get().len())}</p>
            </div>
            {move || signals.pagination_incomplete.get().then(|| view! {
                <p class="stale-banner" role="status">"Application list incomplete: the dashboard stopped at a safe pagination bound or repeated cursor. Use piquelctl for the complete list."</p>
            })}
            {move || match signals.data_state.get() {
                DataState::Loading => view! { <p class="state-panel" role="status">"Loading applications…"</p> }.into_view(),
                DataState::Empty => view! { <div class="state-panel"><h3>"No applications"</h3><p>"The daemon has no desired applications yet. Use piquelctl to create one."</p></div> }.into_view(),
                DataState::Ready | DataState::Stale => ().into_view(),
            }}
            <ul class="application-list" aria-label="Application list">
                {move || signals.applications.get().into_iter().map(|row| application_card(&row, signals, Rc::clone(&select))).collect_view()}
            </ul>
        </section>
    }
    .into_view()
}

fn application_card(
    row: &ApplicationRow,
    signals: DashboardSignals,
    select: SelectApplication,
) -> View {
    let id = row.application.application.id.to_string();
    let id_for_select = id.clone();
    let id_for_class = id.clone();
    let selected_id = signals.selected_id;
    let name = row.application.application.metadata.name.clone();
    let health = row_health(row);
    let status_text = row_status_text(row);
    let desired_replicas = desired_replicas(&row.application);
    view! {
        <li>
            <article class="application-card" class:selected=move || selected_id.get().as_deref() == Some(id_for_class.as_str())>
                <div class="card-topline">
                    <span class=format!("health-badge {}", health_class(health))>{health.label()}</span>
                    <span class="generation">{format!("Generation {}", row.application.generation)}</span>
                </div>
                <h3>{name}</h3>
                <p class="muted"><code>{short_id(&id)}</code></p>
                <dl class="card-facts">
                    <div><dt>"Desired replicas"</dt><dd>{desired_replicas}</dd></div>
                    <div><dt>"Observed state"</dt><dd>{status_text}</dd></div>
                </dl>
                <button class="button button-secondary card-action" type="button" aria-pressed=move || selected_id.get().as_deref() == Some(id.as_str()) on:click=move |_| select(id_for_select.clone())>{"View details"}</button>
            </article>
        </li>
    }
    .into_view()
}

#[component]
fn ApplicationDetail(signals: DashboardSignals, on_manage: Callback<String>) -> impl IntoView {
    view! {
        <section class="detail-panel" aria-labelledby="detail-title">
            <div class="section-heading">
                <div>
                    <p class="eyebrow">"OBSERVED STATE"</p>
                    <h2 id="detail-title">"Application detail"</h2>
                </div>
            </div>
            {move || {
                let selected = signals.selected_id.get();
                let detail = signals.detail.get();
                if selected.is_none() {
                    view! { <p class="state-panel">"Select an application to inspect desired and observed state."</p> }.into_view()
                } else if signals.detail_loading.get() && detail.is_none() {
                    view! { <p class="state-panel" role="status">"Loading application detail…"</p> }.into_view()
                } else if let Some(detail) = detail {
                    detail_view(&detail, signals, on_manage)
                } else {
                    let message = signals.detail_error.get().unwrap_or_else(|| "Application detail is unavailable.".into());
                    view! { <div class="state-panel" role="alert"><h3>"Detail unavailable"</h3><p>{message}</p></div> }.into_view()
                }
            }}
        </section>
    }
    .into_view()
}

#[allow(clippy::too_many_lines)]
fn detail_view(
    detail: &ApplicationDetailView,
    signals: DashboardSignals,
    on_manage: Callback<String>,
) -> View {
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
                Source::Git {
                    repository,
                    reference,
                    ..
                } => format!("git:{repository}#{reference}"),
            };
            let replicas = service.replicas;
            view! {
                <li class="service-row">
                    <div><strong>{name}</strong><span class="muted">{image}</span></div>
                    <span class="replica-pill">{format!("{replicas} desired")}</span>
                </li>
            }
        })
        .collect_view();
    let observed_services = observed
        .services
        .iter()
        .map(observed_service_view)
        .collect_view();
    let runtime_services = status
        .services
        .iter()
        .map(service_status_view)
        .collect_view();
    let diagnostics_view = diagnostics.iter().map(diagnostic_view).collect_view();
    let operation_view = operation.map(|operation| {
        let steps = operation.steps.iter().map(|step| {
            let step_state = step.state.clone();
            view! { <li><span>{step.action.clone()}</span><span class="muted">{step_state}</span></li> }
        }).collect_view();
        view! {
            <section class="subsection" aria-labelledby="operation-title">
                <h3 id="operation-title">"Latest operation"</h3>
                <p><strong>{operation.kind.clone()}</strong>" · "{operation.state.clone()}</p>
                <ul class="step-list">{steps}</ul>
            </section>
        }
    });
    let health = ApplicationHealth::from_server_state(&status.state);
    let manage_id = application_id.clone();
    let refresh_detail = {
        let id = application_id.clone();
        let client = Client::browser();
        move || load_detail(client.clone(), signals, id.clone())
    };
    view! {
        <div class="detail-content">
            {move || signals.detail_error.get().map(|message| view! {
                <p class="stale-banner" role="status">
                    {format!("Showing the last successful detail; the latest detail refresh failed: {message}")}
                </p>
            })}
            <div class="detail-title-row">
                <div><h3>{application_name}</h3><p class="muted"><code>{application_id.clone()}</code></p></div>
                <span class=format!("health-badge {}", health_class(health))>{health.label()}</span>
            </div>
            <div class="detail-actions">
                <button class="button button-primary" type="button" on:click=move |_| on_manage.call(manage_id.clone())>"Manage application"</button>
            </div>
            <dl class="detail-facts">
                <div><dt>"Desired generation"</dt><dd>{detail.application.generation}</dd></div>
                <div><dt>"Observed generation"</dt><dd>{status.observed_generation.map_or_else(|| "Not observed".into(), |generation| generation.to_string())}</dd></div>
                <div><dt>"Networks / volumes"</dt><dd>{format!("{} / {}", observed.network_count, observed.volume_count)}</dd></div>
                <div><dt>"Ingress"</dt><dd>{status.infrastructure.clone().unwrap_or_else(|| "not used".into())}</dd></div>
                <div><dt>"Routes"</dt><dd>{app.spec.routes.len()}</dd></div>
            </dl>
            <p class="detail-state"><span class=format!("health-badge {}", health_class(health))>{health.label()}</span> {status.message.clone().unwrap_or_else(|| "No additional daemon diagnostic.".into())}</p>
            <div class="detail-refresh"><button class="button button-secondary" type="button" on:click=move |_| refresh_detail()>{"Refresh detail"}</button></div>
            <section class="subsection" aria-labelledby="desired-title">
                <h3 id="desired-title">"Desired services"</h3>
                <ul class="service-list">{desired_services}</ul>
            </section>
            <section class="subsection" aria-labelledby="observed-title">
                <h3 id="observed-title">"Observed services"</h3>
                <ul class="service-list">{observed_services}</ul>
            </section>
            <section class="subsection" aria-labelledby="runtime-title">
                <h3 id="runtime-title">"Runtime status"</h3>
                {if status.services.is_empty() {
                    view! { <p class="muted">"Runtime status is not available yet."</p> }.into_view()
                } else {
                    view! { <ul class="service-list">{runtime_services}</ul> }.into_view()
                }}
            </section>
            <section class="subsection" aria-labelledby="diagnostic-title">
                <h3 id="diagnostic-title">"Reconciliation diagnostics"</h3>
                {if diagnostics.is_empty() {
                    view! { <p class="muted">"No diagnostics reported."</p> }.into_view()
                } else {
                    view! { <ul class="diagnostic-list">{diagnostics_view}</ul> }.into_view()
                }}
            </section>
            {operation_view}
            <ApplicationObservability application_id=application_id.clone()/>
        </div>
    }
    .into_view()
}

fn service_status_view(service: &ServiceStatusView) -> View {
    let diagnostic = service.diagnostic.clone();
    view! {
        <li class="service-row service-observed">
            <div><strong>{service.service.clone()}</strong><span class="muted">{service.state.clone()}</span></div>
            <div class="observed-replicas">{format!("{} / {} running", service.running_replicas, service.desired_replicas)}</div>
            {diagnostic.map(|message| view! { <p class="inline-diagnostic">{message}</p> })}
        </li>
    }.into_view()
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
        <li class="service-row service-observed">
            <div><strong>{service.name.clone()}</strong><span class="muted">{image}</span></div>
            <div class="observed-replicas"><span class=format!("health-badge {}", health_class(health))>{health.label()}</span><span>{format!("{} / {} healthy", service.healthy_replicas, service.desired_replicas)}</span></div>
            {(!service.diagnostics.is_empty()).then(|| view! { <ul class="inline-diagnostics">{diagnostics}</ul> })}
        </li>
    }
    .into_view()
}

fn diagnostic_view(diagnostic: &DiagnosticView) -> View {
    view! { <li><strong>{diagnostic.code.clone()}</strong><span>{diagnostic.message.clone()}</span></li> }
        .into_view()
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
        for application in page.items {
            let id = application.application.id.to_string();
            match client.application_status(&id).await {
                Ok(status) => applications.push(ApplicationRow {
                    application,
                    status: Some(status),
                    status_error: None,
                }),
                Err(error) => applications.push(ApplicationRow {
                    application,
                    status: None,
                    status_error: Some(client_error_message(&error)),
                }),
            }
        }
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
        ClientError::Endpoint => "The dashboard endpoint is invalid.".into(),
        ClientError::Transport { message } => format!("Could not reach piqueld: {message}"),
        ClientError::Api { error, .. } => error.message.clone(),
        ClientError::Decode => "The daemon returned an invalid public API response.".into(),
        ClientError::SecretFile => "The browser cannot use a protected native secret file.".into(),
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
        ApplicationHealth::Converged => "health-converged",
        ApplicationHealth::Degraded => "health-degraded",
        ApplicationHealth::Failed => "health-failed",
        ApplicationHealth::Pending => "health-pending",
    }
}

fn short_id(value: &str) -> String {
    value.chars().take(12).collect()
}

#[allow(clippy::too_many_lines)]
fn manifest_from_normalized(application: &NormalizedApplication) -> ApplicationManifest {
    ApplicationManifest {
        api_version: application.api_version.clone(),
        kind: application.kind.clone(),
        metadata: MetadataInput {
            name: application.metadata.name.clone(),
        },
        spec: ApplicationSpecInput {
            services: application
                .spec
                .services
                .iter()
                .map(|service| ServiceInput {
                    name: service.name.clone(),
                    source: match &service.source {
                        Source::Image { image } => SourceInput::Image {
                            image: image.clone(),
                        },
                        Source::Git {
                            repository,
                            reference,
                            context,
                            dockerfile,
                        } => SourceInput::Git {
                            repository: repository.clone(),
                            reference: reference.clone(),
                            context: context.clone(),
                            dockerfile: dockerfile.clone(),
                        },
                    },
                    replicas: service.replicas,
                    environment: service.environment.clone(),
                    command: service.command.clone(),
                    arguments: service.arguments.clone(),
                    ports: service.ports.clone(),
                    mounts: service
                        .mounts
                        .iter()
                        .map(|mount| MountInput {
                            volume: mount.volume.clone(),
                            target: mount.target.clone(),
                            read_only: mount.read_only,
                        })
                        .collect(),
                    secrets: service
                        .secrets
                        .iter()
                        .map(|secret| SecretReferenceInput {
                            source: secret.source.clone(),
                            target: Some(secret.target.clone()),
                            mode: secret.mode.clone(),
                        })
                        .collect(),
                    healthcheck: service.healthcheck.as_ref().map(|health| match health {
                        HealthCheck::Http {
                            port,
                            path,
                            interval_seconds,
                            timeout_seconds,
                        } => HealthCheckInput::Http {
                            port: *port,
                            path: path.clone(),
                            interval_seconds: *interval_seconds,
                            timeout_seconds: *timeout_seconds,
                        },
                        HealthCheck::Command {
                            command,
                            interval_seconds,
                            timeout_seconds,
                        } => HealthCheckInput::Command {
                            command: command.clone(),
                            interval_seconds: *interval_seconds,
                            timeout_seconds: *timeout_seconds,
                        },
                    }),
                    resources: service
                        .resources
                        .as_ref()
                        .map(|resources| ResourceLimitsInput {
                            cpu_millis: resources.cpu_millis,
                            memory_bytes: resources.memory_bytes,
                        }),
                })
                .collect(),
            volumes: application
                .spec
                .volumes
                .iter()
                .map(|volume| VolumeInput {
                    name: volume.name.clone(),
                })
                .collect(),
            routes: application
                .spec
                .routes
                .iter()
                .map(|route| RouteInput {
                    host: route.host.clone(),
                    service: route.service.clone(),
                    port: route.port,
                })
                .collect(),
        },
    }
}

fn form_lines(value: &str) -> Vec<String> {
    value
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

fn form_ports(value: &str) -> Vec<u16> {
    value
        .split(',')
        .filter_map(|part| part.trim().parse().ok())
        .collect()
}

fn form_environment(value: &str) -> BTreeMap<String, String> {
    value
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_owned(), value.to_owned()))
        .collect()
}

fn environment_text(service: &ServiceInput) -> String {
    service
        .environment
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn ports_text(service: &ServiceInput) -> String {
    service
        .ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn volumes_text(manifest: &ApplicationManifest) -> String {
    manifest
        .spec
        .volumes
        .iter()
        .map(|volume| volume.name.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

fn routes_text(manifest: &ApplicationManifest) -> String {
    manifest
        .spec
        .routes
        .iter()
        .map(|route| format!("{},{},{}", route.host, route.service, route.port))
        .collect::<Vec<_>>()
        .join("\n")
}

fn mounts_text(service: &ServiceInput) -> String {
    service
        .mounts
        .iter()
        .map(|mount| {
            format!(
                "{}:{}{}",
                mount.volume,
                mount.target,
                if mount.read_only { ":ro" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn secrets_text(service: &ServiceInput) -> String {
    service
        .secrets
        .iter()
        .map(|secret| {
            format!(
                "{}:{}:{}",
                secret.source,
                secret.target.as_deref().unwrap_or_default(),
                secret.mode
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_mounts(value: &str) -> Vec<MountInput> {
    form_lines(value)
        .into_iter()
        .filter_map(|line| {
            let mut parts = line.split(':');
            Some(MountInput {
                volume: parts.next()?.to_owned(),
                target: parts.next()?.to_owned(),
                read_only: parts.next() == Some("ro"),
            })
        })
        .collect()
}

fn parse_secret_references(value: &str) -> Vec<SecretReferenceInput> {
    form_lines(value)
        .into_iter()
        .filter_map(|line| {
            let mut parts = line.split(':');
            Some(SecretReferenceInput {
                source: parts.next()?.to_owned(),
                target: parts
                    .next()
                    .filter(|target| !target.is_empty())
                    .map(str::to_owned),
                mode: parts.next().unwrap_or("0400").to_owned(),
            })
        })
        .collect()
}

fn parse_routes(value: &str) -> Vec<RouteInput> {
    form_lines(value)
        .into_iter()
        .filter_map(|line| {
            let parts = line.split(',').map(str::trim).collect::<Vec<_>>();
            Some(RouteInput {
                host: (*parts.first()?).to_owned(),
                service: (*parts.get(1)?).to_owned(),
                port: parts.get(2)?.parse().ok()?,
            })
        })
        .collect()
}

fn health_kind(service: &ServiceInput) -> &'static str {
    match service.healthcheck {
        Some(HealthCheckInput::Http { .. }) => "http",
        Some(HealthCheckInput::Command { .. }) => "command",
        None => "none",
    }
}

fn health_text(service: &ServiceInput) -> String {
    match &service.healthcheck {
        Some(HealthCheckInput::Http {
            port,
            path,
            interval_seconds,
            timeout_seconds,
        }) => format!("{port},{path},{interval_seconds},{timeout_seconds}"),
        Some(HealthCheckInput::Command {
            command,
            interval_seconds,
            timeout_seconds,
        }) => format!(
            "{}|{interval_seconds}|{timeout_seconds}",
            command.join("\n")
        ),
        None => String::new(),
    }
}

fn update_healthcheck(service: &mut ServiceInput, text: &str) {
    service.healthcheck = match service.healthcheck.clone() {
        Some(HealthCheckInput::Http { .. }) => {
            let parts = text.split(',').collect::<Vec<_>>();
            Some(HealthCheckInput::Http {
                port: parts
                    .first()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(8080),
                path: parts.get(1).unwrap_or(&"/health").to_string(),
                interval_seconds: parts
                    .get(2)
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(10),
                timeout_seconds: parts
                    .get(3)
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(3),
            })
        }
        Some(HealthCheckInput::Command { .. }) => {
            let parts = text.split('|').collect::<Vec<_>>();
            Some(HealthCheckInput::Command {
                command: parts
                    .first()
                    .map_or_else(Vec::new, |value| form_lines(value)),
                interval_seconds: parts
                    .get(1)
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(10),
                timeout_seconds: parts
                    .get(2)
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(3),
            })
        }
        None => None,
    };
}

fn update_cpu_limit(service: &mut ServiceInput, cpu_millis: Option<u32>) {
    let memory_bytes = service
        .resources
        .as_ref()
        .and_then(|resources| resources.memory_bytes);
    service.resources =
        (cpu_millis.is_some() || memory_bytes.is_some()).then_some(ResourceLimitsInput {
            cpu_millis,
            memory_bytes,
        });
}

fn update_memory_limit(service: &mut ServiceInput, memory_bytes: Option<u64>) {
    let cpu_millis = service
        .resources
        .as_ref()
        .and_then(|resources| resources.cpu_millis);
    service.resources =
        (cpu_millis.is_some() || memory_bytes.is_some()).then_some(ResourceLimitsInput {
            cpu_millis,
            memory_bytes,
        });
}

fn error_is_generation_conflict(error: &ClientError) -> bool {
    matches!(error, ClientError::Api { error, .. } if error.code == "application_generation_conflict")
}

fn conflict_generation(error: &ClientError, fallback: u64) -> u64 {
    match error {
        ClientError::Api { error, .. } => error
            .details
            .get("current_generation")
            .and_then(Value::as_u64)
            .unwrap_or(fallback),
        _ => fallback,
    }
}

#[component]
fn ErrorNotice(notice: RwSignal<Notice>) -> impl IntoView {
    view! {
        {move || (!notice.get().message.is_empty()).then(|| {
            let current = notice.get();
            view! {
                <p class:error=current.error class="notice" role=if current.error { "alert" } else { "status" }>
                    {current.message}
                </p>
            }
        })}
    }
}

#[component]
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
fn ApplicationEditor(application_id: Option<String>, on_done: Callback<()>) -> impl IntoView {
    let editing = application_id.is_some();
    let manifest = create_rw_signal(blank_manifest());
    let generation = create_rw_signal(0_u64);
    let notice = create_rw_signal(Notice::default());
    let field_errors = create_rw_signal(FieldErrors::default());
    let plan = create_rw_signal(None::<PlanView>);
    let planned_manifest = create_rw_signal(None::<ApplicationManifest>);
    let delete_plan = create_rw_signal(None::<PlanView>);
    let conflict = create_rw_signal(None::<ConflictState>);
    let busy = create_rw_signal(editing);
    let operation = create_rw_signal(None::<String>);
    let client = Client::browser();

    if let Some(id) = application_id.clone() {
        let client = client.clone();
        spawn_local(async move {
            match client.application(&id).await {
                Ok(view) => {
                    generation.set(view.generation);
                    manifest.set(manifest_from_normalized(&view.application));
                }
                Err(error) => notice.set(Notice {
                    message: client_error_message(&error),
                    error: true,
                }),
            }
            busy.set(false);
        });
    }

    let apply = {
        let client = client.clone();
        let id = application_id.clone();
        Callback::new(move |()| {
            if busy.get_untracked() {
                return;
            }
            let local = manifest.get_untracked();
            if planned_manifest.get_untracked().as_ref() != Some(&local) {
                plan.set(None);
                planned_manifest.set(None);
                notice.set(Notice {
                    message: "The form changed after this preview. Preview the current form before applying.".into(),
                    error: true,
                });
                return;
            }
            busy.set(true);
            let expected = generation.get_untracked();
            let client = client.clone();
            let id = id.clone();
            spawn_local(async move {
                let result = if let Some(id) = id {
                    client
                        .replace_application(
                            &id,
                            &ReplaceApplicationRequest {
                                expected_generation: expected,
                                manifest: local.clone(),
                            },
                        )
                        .await
                } else {
                    let key = format!("ui-{}-{}", js_sys::Date::now(), local.metadata.name);
                    client
                        .create_application(
                            &CreateApplicationRequest {
                                manifest: local.clone(),
                            },
                            &key,
                        )
                        .await
                };
                match result {
                    Ok(accepted) => {
                        operation.set(Some(accepted.operation_id));
                        generation.set(accepted.generation);
                        plan.set(None);
                        planned_manifest.set(None);
                        notice.set(Notice {
                            message: "Operation accepted. Follow its bounded progress below."
                                .into(),
                            error: false,
                        });
                    }
                    Err(error) if error_is_generation_conflict(&error) => {
                        conflict.set(Some(ConflictState {
                            current_generation: conflict_generation(&error, expected),
                            local_manifest: local,
                        }));
                        notice.set(Notice {
                            message:
                                "Another client changed this application. Your edits are preserved."
                                    .into(),
                            error: true,
                        });
                    }
                    Err(error) => notice.set(Notice {
                        message: client_error_message(&error),
                        error: true,
                    }),
                }
                busy.set(false);
            });
        })
    };

    let preview = {
        let client = client.clone();
        let id = application_id.clone();
        Callback::new(move |()| {
            if busy.get_untracked() {
                return;
            }
            busy.set(true);
            field_errors.set(FieldErrors::default());
            let local = manifest.get_untracked();
            let previewed = local.clone();
            let expected = generation.get_untracked();
            let client = client.clone();
            let id = id.clone();
            spawn_local(async move {
                let result = if let Some(id) = id {
                    client
                        .plan_replace(
                            &id,
                            &ReplacePlanRequest {
                                expected_generation: expected,
                                manifest: local,
                            },
                        )
                        .await
                } else {
                    client
                        .plan_create(&PlanApplicationRequest { manifest: local })
                        .await
                };
                match result {
                    Ok(value) => {
                        planned_manifest.set(Some(previewed));
                        plan.set(Some(value));
                        notice.set(Notice {
                            message: "Plan is ready for review.".into(),
                            error: false,
                        });
                    }
                    Err(error) => {
                        planned_manifest.set(None);
                        if let ClientError::Api { error: body, .. } = &error {
                            field_errors.set(FieldErrors::from_details(&body.details));
                        }
                        notice.set(Notice {
                            message: client_error_message(&error),
                            error: true,
                        });
                    }
                }
                busy.set(false);
            });
        })
    };

    let reload = {
        let client = client.clone();
        let id = application_id.clone();
        Callback::new(move |()| {
            let Some(id) = id.clone() else { return };
            busy.set(true);
            let client = client.clone();
            spawn_local(async move {
                match client.application(&id).await {
                    Ok(view) => {
                        generation.set(view.generation);
                        manifest.set(manifest_from_normalized(&view.application));
                        conflict.set(None);
                        plan.set(None);
                        planned_manifest.set(None);
                        field_errors.set(FieldErrors::default());
                        notice.set(Notice {
                            message: "Reloaded the current server version.".into(),
                            error: false,
                        });
                    }
                    Err(error) => notice.set(Notice {
                        message: client_error_message(&error),
                        error: true,
                    }),
                }
                busy.set(false);
            });
        })
    };

    let preview_delete = {
        let client = client.clone();
        let id = application_id.clone();
        Callback::new(move |()| {
            let Some(id) = id.clone() else { return };
            busy.set(true);
            let expected = generation.get_untracked();
            let client = client.clone();
            spawn_local(async move {
                match client
                    .plan_delete(
                        &id,
                        &DeleteApplicationRequest {
                            expected_generation: expected,
                        },
                    )
                    .await
                {
                    Ok(value) => {
                        delete_plan.set(Some(value));
                        notice.set(Notice {
                            message:
                                "Deletion plan is ready. Review every action before confirming."
                                    .into(),
                            error: false,
                        });
                    }
                    Err(error) if error_is_generation_conflict(&error) => {
                        conflict.set(Some(ConflictState {
                            current_generation: conflict_generation(&error, expected),
                            local_manifest: manifest.get_untracked(),
                        }));
                        notice.set(Notice {
                            message: "The application changed. Your local edits are preserved; reload before deleting.".into(),
                            error: true,
                        });
                    }
                    Err(error) => notice.set(Notice {
                        message: client_error_message(&error),
                        error: true,
                    }),
                }
                busy.set(false);
            });
        })
    };

    let confirm_delete = {
        let client = client.clone();
        let id = application_id.clone();
        Callback::new(move |()| {
            if busy.get_untracked()
                || !browser_window().is_some_and(|window| {
                    window
                        .confirm_with_message(
                            "Confirm the reviewed deletion plan? Named volumes are retained.",
                        )
                        .unwrap_or(false)
                })
            {
                return;
            }
            let Some(id) = id.clone() else { return };
            busy.set(true);
            let expected = generation.get_untracked();
            let client = client.clone();
            spawn_local(async move {
                match client
                    .delete_application(
                        &id,
                        &DeleteApplicationRequest {
                            expected_generation: expected,
                        },
                    )
                    .await
                {
                    Ok(accepted) => {
                        operation.set(Some(accepted.operation_id));
                        generation.set(accepted.generation);
                        delete_plan.set(None);
                        notice.set(Notice {
                            message: "Deletion accepted. Runtime removal is being reconciled."
                                .into(),
                            error: false,
                        });
                    }
                    Err(error) if error_is_generation_conflict(&error) => {
                        delete_plan.set(None);
                        conflict.set(Some(ConflictState {
                            current_generation: conflict_generation(&error, expected),
                            local_manifest: manifest.get_untracked(),
                        }));
                        notice.set(Notice {
                            message:
                                "The deletion preview is stale; your local form remains intact."
                                    .into(),
                            error: true,
                        });
                    }
                    Err(error) => notice.set(Notice {
                        message: client_error_message(&error),
                        error: true,
                    }),
                }
                busy.set(false);
            });
        })
    };

    view! {
        <section class="editor" aria-labelledby="editor-title">
            <div class="page-heading">
                <div><p class="eyebrow">{if editing { "Edit safely" } else { "New desired state" }}</p><h1 id="editor-title">{if editing { "Manage application" } else { "Create application" }}</h1></div>
                <button class="button button-secondary" type="button" on:click=move |_| on_done.call(())>"Close"</button>
            </div>
            <ErrorNotice notice=notice/>
            {move || busy.get().then(|| view! { <p class="loading" role="status">"Working…"</p> })}
            <form autocomplete="off" on:submit=|event| event.prevent_default()>
                <fieldset>
                    <legend>"Identity"</legend>
                    <label for="app-name">"Application name"</label>
                    <input id="app-name" required maxlength="63" prop:value=move || manifest.get().metadata.name on:input=move |event| manifest.update(|value| value.metadata.name = event_target_value(&event))/>
                    <FieldMessages errors=field_errors path="metadata.name".into()/>
                </fieldset>
                <h2>"Services"</h2>
                {move || (0..manifest.get().spec.services.len()).map(|index| view! { <ServiceFields manifest=manifest index=index errors=field_errors/> }).collect_view()}
                <button class="button button-secondary" type="button" on:click=move |_| manifest.update(|value| value.spec.services.push(blank_service()))>"Add service"</button>
                <CollectionFields manifest=manifest errors=field_errors/>
                <div class="actions">
                    <button class="button button-primary" type="button" disabled=move || busy.get() on:click=move |_| preview.call(())>"Preview plan"</button>
                </div>
            </form>
            {move || plan.get().map(|value| view! { <PlanPanel plan=value on_apply=apply/> })}
            {move || conflict.get().map(|value| view! {
                <section class="warning" role="alert" tabindex="-1">
                    <h2>"Generation conflict"</h2>
                    <p>{format!("The server is now at generation {}. Your local form has not been changed.", value.current_generation)}</p>
                    <div class="actions"><button class="button button-secondary" type="button" on:click=move |_| conflict.set(None)>"Keep editing"</button><button class="button button-secondary" type="button" on:click=move |_| reload.call(())>"Reload server version"</button></div>
                </section>
            })}
            {move || operation.get().map(|id| view! { <OperationProgress operation_id=id/> })}
            {editing.then(|| view! {
                <section class="danger-zone" aria-labelledby="delete-title">
                    <h2 id="delete-title">"Delete application"</h2>
                    <p>"Preview the server-observed removal plan. Named volumes are retained by policy."</p>
                    <button class="button button-secondary" type="button" disabled=move || busy.get() on:click=move |_| preview_delete.call(())>"Preview deletion plan"</button>
                    {move || delete_plan.get().map(|value| view! { <DeletePlanPanel plan=value on_confirm=confirm_delete/> })}
                </section>
            })}
        </section>
    }
}

#[component]
fn FieldMessages(errors: RwSignal<FieldErrors>, path: String) -> impl IntoView {
    view! { <div class="field-error" role="alert">{move || errors.get().messages(&path).join(". ")}</div> }
}

#[component]
fn FieldMessagesUnder(errors: RwSignal<FieldErrors>, prefix: String) -> impl IntoView {
    view! {
        <ul class="field-error" role="alert">
            {move || errors.get().under(&prefix).into_iter().map(|(path, message)| view! { <li><code>{path}</code>" — "{message}</li> }).collect_view()}
        </ul>
    }
}

#[component]
fn ServiceFields(
    manifest: RwSignal<ApplicationManifest>,
    index: usize,
    errors: RwSignal<FieldErrors>,
) -> impl IntoView {
    let service = move || {
        manifest
            .get()
            .spec
            .services
            .get(index)
            .cloned()
            .unwrap_or_else(blank_service)
    };
    let field_prefix = format!("spec.services[{index}]");
    let source_kind = move || service().source;
    view! {
        <fieldset class="service">
            <legend>{move || format!("Service {}", index + 1)}</legend>
            <div class="grid two">
                <label>"Name"<input required prop:value=move || service().name on:input=move |event| manifest.update(|value| if let Some(service) = value.spec.services.get_mut(index) { service.name = event_target_value(&event); })/></label>
                <label>"Replicas"<input type="number" min="1" max="100" prop:value=move || service().replicas on:input=move |event| if let Ok(replicas) = event_target_value(&event).parse() { manifest.update(|value| if let Some(service) = value.spec.services.get_mut(index) { service.replicas = replicas; }) }/></label>
            </div>
            <FieldMessagesUnder errors=errors prefix=field_prefix.clone()/>
            <label>"Source type"
                <select on:change=move |event| { let git = event_target_value(&event) == "git"; manifest.update(|value| if let Some(service) = value.spec.services.get_mut(index) { set_source_kind(service, git); }); }>
                    <option value="image" selected=move || matches!(source_kind(), SourceInput::Image { .. })>"Prebuilt image"</option>
                    <option value="git" selected=move || matches!(source_kind(), SourceInput::Git { .. })>"Git repository"</option>
                </select>
            </label>
            {move || match service().source {
                SourceInput::Image { image } => view! { <label>"Image reference"<input required prop:value=image on:input=move |event| manifest.update(|value| if let Some(ServiceInput { source: SourceInput::Image { image }, .. }) = value.spec.services.get_mut(index) { *image = event_target_value(&event); })/></label> }.into_view(),
                SourceInput::Git { repository, reference, context, dockerfile } => view! {
                    <div class="grid">
                        <label>"Repository URL"<input required prop:value=repository on:input=move |event| manifest.update(|value| if let Some(ServiceInput { source: SourceInput::Git { repository, .. }, .. }) = value.spec.services.get_mut(index) { *repository = event_target_value(&event); })/></label>
                        <label>"Git reference"<input prop:value=reference on:input=move |event| manifest.update(|value| if let Some(ServiceInput { source: SourceInput::Git { reference, .. }, .. }) = value.spec.services.get_mut(index) { *reference = event_target_value(&event); })/></label>
                        <label>"Build context"<input prop:value=context on:input=move |event| manifest.update(|value| if let Some(ServiceInput { source: SourceInput::Git { context, .. }, .. }) = value.spec.services.get_mut(index) { *context = event_target_value(&event); })/></label>
                        <label>"Dockerfile"<input prop:value=dockerfile on:input=move |event| manifest.update(|value| if let Some(ServiceInput { source: SourceInput::Git { dockerfile, .. }, .. }) = value.spec.services.get_mut(index) { *dockerfile = event_target_value(&event); })/></label>
                    </div>
                }.into_view(),
            }}
            <div class="grid">
                <label>"Environment (KEY=value, one per line)"<textarea rows="4" prop:value=move || environment_text(&service()) on:input=move |event| manifest.update(|value| if let Some(service) = value.spec.services.get_mut(index) { service.environment = form_environment(&event_target_value(&event)); })></textarea></label>
                <div>
                    <label>"Ports (comma separated)"<input inputmode="numeric" prop:value=move || ports_text(&service()) on:input=move |event| manifest.update(|value| if let Some(service) = value.spec.services.get_mut(index) { service.ports = form_ports(&event_target_value(&event)); })/></label>
                    <label>"Command (one argument per line)"<textarea rows="2" prop:value=move || service().command.join("\n") on:input=move |event| manifest.update(|value| if let Some(service) = value.spec.services.get_mut(index) { service.command = form_lines(&event_target_value(&event)); })></textarea></label>
                    <label>"Arguments (one per line)"<textarea rows="2" prop:value=move || service().arguments.join("\n") on:input=move |event| manifest.update(|value| if let Some(service) = value.spec.services.get_mut(index) { service.arguments = form_lines(&event_target_value(&event)); })></textarea></label>
                </div>
            </div>
            <details>
                <summary>"Mounts, secrets, health, and resources"</summary>
                <label>"Mounts (volume:target[:ro], one per line)"<textarea rows="3" prop:value=move || mounts_text(&service()) on:input=move |event| manifest.update(|value| if let Some(service) = value.spec.services.get_mut(index) { service.mounts = parse_mounts(&event_target_value(&event)); })></textarea></label>
                <label>"Secret files (logical-name:target:mode, one per line)"<textarea rows="3" prop:value=move || secrets_text(&service()) on:input=move |event| manifest.update(|value| if let Some(service) = value.spec.services.get_mut(index) { service.secrets = parse_secret_references(&event_target_value(&event)); })></textarea></label>
                <div class="grid two">
                    <label>"Health check"
                        <select on:change=move |event| manifest.update(|value| if let Some(service) = value.spec.services.get_mut(index) { service.healthcheck = match event_target_value(&event).as_str() { "http" => Some(HealthCheckInput::Http { port: 8080, path: "/health".into(), interval_seconds: 10, timeout_seconds: 3 }), "command" => Some(HealthCheckInput::Command { command: vec!["true".into()], interval_seconds: 10, timeout_seconds: 3 }), _ => None }; })>
                            <option value="none" selected=move || health_kind(&service()) == "none">"None"</option>
                            <option value="http" selected=move || health_kind(&service()) == "http">"HTTP"</option>
                            <option value="command" selected=move || health_kind(&service()) == "command">"Command"</option>
                        </select>
                    </label>
                    <label>"Health details (HTTP: port,path,interval,timeout; command: command|interval|timeout)"<input prop:value=move || health_text(&service()) on:input=move |event| { let text = event_target_value(&event); manifest.update(|value| if let Some(service) = value.spec.services.get_mut(index) { update_healthcheck(service, &text); }); }/></label>
                </div>
                <div class="grid two">
                    <label>"CPU limit (millicores)"<input type="number" min="1" prop:value=move || service().resources.as_ref().and_then(|value| value.cpu_millis).map_or_else(String::new, |value| value.to_string()) on:input=move |event| { let cpu = event_target_value(&event).parse().ok(); manifest.update(|value| if let Some(service) = value.spec.services.get_mut(index) { update_cpu_limit(service, cpu); }); }/></label>
                    <label>"Memory limit (bytes)"<input type="number" min="1" prop:value=move || service().resources.as_ref().and_then(|value| value.memory_bytes).map_or_else(String::new, |value| value.to_string()) on:input=move |event| { let memory = event_target_value(&event).parse().ok(); manifest.update(|value| if let Some(service) = value.spec.services.get_mut(index) { update_memory_limit(service, memory); }); }/></label>
                </div>
            </details>
            <button class="button button-secondary" type="button" disabled=move || manifest.get().spec.services.len() <= 1 on:click=move |_| manifest.update(|value| { if index < value.spec.services.len() { value.spec.services.remove(index); } })>"Remove service"</button>
        </fieldset>
    }
}

#[component]
fn CollectionFields(
    manifest: RwSignal<ApplicationManifest>,
    errors: RwSignal<FieldErrors>,
) -> impl IntoView {
    view! {
        <div class="grid two">
            <fieldset><legend>"Named volumes"</legend><p class="help">"One name per line. Volumes are retained when an application is deleted."</p><textarea rows="5" prop:value=move || volumes_text(&manifest.get()) on:input=move |event| manifest.update(|value| value.spec.volumes = form_lines(&event_target_value(&event)).into_iter().map(|name| VolumeInput { name }).collect())></textarea><FieldMessagesUnder errors=errors prefix="spec.volumes".into()/></fieldset>
            <fieldset><legend>"HTTP host routes"</legend><p class="help">"host,service,port — one route per line"</p><textarea rows="5" prop:value=move || routes_text(&manifest.get()) on:input=move |event| manifest.update(|value| value.spec.routes = parse_routes(&event_target_value(&event)))></textarea><FieldMessagesUnder errors=errors prefix="spec.routes".into()/></fieldset>
        </div>
    }
}

#[component]
fn PlanPanel(plan: PlanView, on_apply: Callback<()>) -> impl IntoView {
    let destructive = plan.plan.actions.iter().any(|action| action.destructive);
    let blocked = plan.plan.is_blocked();
    view! {
        <section class="plan" aria-labelledby="plan-title">
            <div class="page-heading"><div><p class="eyebrow">"Proposed generation"</p><h2 id="plan-title">{plan.proposed_generation}</h2></div><span class="badge">{if destructive { "Destructive actions" } else { "Conservative plan" }}</span></div>
            <PlanActions actions=plan.plan.actions diagnostics=plan.plan.diagnostics/>
            <p class="help">"Review every action. Named-volume retention is explicit; apply starts resolution, builds, and reconciliation."</p>
            <button class="button button-primary" type="button" disabled=blocked on:click=move |_| on_apply.call(())>"Apply this plan"</button>
        </section>
    }
}

#[component]
fn DeletePlanPanel(plan: PlanView, on_confirm: Callback<()>) -> impl IntoView {
    let blocked = plan.plan.is_blocked();
    view! {
        <section class="plan delete-plan" role="region" aria-labelledby="delete-plan-title">
            <div class="page-heading"><div><p class="eyebrow">"Server-observed deletion preview"</p><h3 id="delete-plan-title">{format!("Generation {} deletion actions", plan.proposed_generation)}</h3></div><span class="badge">"Destructive review"</span></div>
            <PlanActions actions=plan.plan.actions diagnostics=plan.plan.diagnostics/>
            <p class="warning"><strong>"Retention policy: "</strong>"named volumes remain in Docker and are not deleted."</p>
            <button class="button button-secondary" type="button" disabled=blocked on:click=move |_| on_confirm.call(())>"Confirm reviewed deletion"</button>
        </section>
    }
}

#[component]
fn PlanActions(
    actions: Vec<piqueld_core::PlanAction>,
    diagnostics: Vec<piqueld_core::PlanDiagnostic>,
) -> impl IntoView {
    view! {
        <div class="plan-actions">
            <ol>{actions.into_iter().map(|action| view! { <li><strong>{action.to_string()}</strong><span>{format!("{:?} · risk: {:?}", action.reason, action.risk)}</span></li> }).collect_view()}</ol>
            {(!diagnostics.is_empty()).then(|| view! { <div class="warning" role="alert"><strong>"Plan diagnostics"</strong><ul>{diagnostics.into_iter().map(|diagnostic| view! { <li>{format!("{}: {}", diagnostic.resource, diagnostic.message)}</li> }).collect_view()}</ul></div> })}
        </div>
    }
}

fn operation_terminal(state: &str) -> bool {
    matches!(state, "succeeded" | "failed" | "cancelled")
}

fn parse_event<T: DeserializeOwned>(event: &MessageEvent) -> Option<T> {
    event
        .data()
        .as_string()
        .and_then(|data| serde_json::from_str(&data).ok())
}

fn log_record_line(record: &ContainerLogView) -> String {
    format!(
        "{} [{}] {}",
        record.timestamp, record.service, record.display_message
    )
}

fn append_log_records(buffer: &mut LogBuffer, records: &[ContainerLogView]) {
    for record in records {
        buffer.push(log_record_line(record));
    }
}

#[component]
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
fn OperationProgress(operation_id: String) -> impl IntoView {
    let events = create_rw_signal({
        let mut value = LogBuffer::default();
        value.follow = true;
        value
    });
    let status = create_rw_signal("Connecting to live operation…".to_owned());
    let operation = create_rw_signal(None::<piqueld_client::OperationView>);
    let builds = create_rw_signal(Vec::<BuildView>::new());
    let reconnect = create_rw_signal(ReconnectState::default());
    let alive = Rc::new(Cell::new(true));
    let client = Client::browser();

    {
        let client = client.clone();
        let id = operation_id.clone();
        let alive = Rc::clone(&alive);
        spawn_local(async move {
            while alive.get() {
                match client.operation(&id).await {
                    Ok(value) => {
                        let terminal = operation_terminal(&value.state);
                        status.set(if terminal {
                            format!("Operation {}", value.state)
                        } else {
                            "Operation in progress".into()
                        });
                        operation.set(Some(value));
                        if let Ok(page) = client.operation_builds(&id).await {
                            builds.set(page.items);
                        }
                        if terminal {
                            break;
                        }
                    }
                    Err(error) => status.set(format!(
                        "Operation polling failed: {}",
                        client_error_message(&error)
                    )),
                }
                TimeoutFuture::new(3_000).await;
            }
        });
    }

    let source = EventSource::new(&format!("{API_PREFIX}/operations/{operation_id}/events")).ok();
    let mut listeners = None;
    if let Some(source_ref) = source.as_ref() {
        let on_operation = {
            Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                reconnect.update(|state| {
                    state.connected();
                    let id = event.last_event_id();
                    state.observed((!id.is_empty()).then_some(id));
                });
                if let Some(value) = parse_event::<piqueld_client::OperationView>(&event) {
                    events.update(|buffer| {
                        buffer.push(format!("{}: {}", value.kind, value.state));
                        for step in &value.steps {
                            buffer.push(format!("  {} — {}", step.action, step.state));
                        }
                    });
                    status.set(if operation_terminal(&value.state) {
                        format!("Operation {}", value.state)
                    } else {
                        "Operation in progress".into()
                    });
                    operation.set(Some(value));
                }
            })
        };
        let _ = source_ref
            .add_event_listener_with_callback("operation", on_operation.as_ref().unchecked_ref());
        let on_terminal = {
            Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                if let Some(value) = parse_event::<piqueld_client::OperationView>(&event) {
                    events.update(|buffer| buffer.push(format!("terminal: {}", value.state)));
                    status.set(format!("Operation {}", value.state));
                    operation.set(Some(value));
                }
            })
        };
        let _ = source_ref
            .add_event_listener_with_callback("terminal", on_terminal.as_ref().unchecked_ref());
        let on_error = {
            Closure::<dyn FnMut()>::new(move || {
                reconnect.update(|state| {
                    if state.disconnected().is_none() {
                        status.set("Live stream paused; bounded polling is active.".into());
                    } else {
                        status.set("Live stream interrupted; reconnecting…".into());
                    }
                });
            })
        };
        source_ref.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        listeners = Some((on_operation, on_terminal, on_error));
    }
    {
        let alive = Rc::clone(&alive);
        on_cleanup(move || {
            alive.set(false);
            if let Some(source) = source {
                if let Some((on_operation, on_terminal, _on_error)) = listeners {
                    let _ = source.remove_event_listener_with_callback(
                        "operation",
                        on_operation.as_ref().unchecked_ref(),
                    );
                    let _ = source.remove_event_listener_with_callback(
                        "terminal",
                        on_terminal.as_ref().unchecked_ref(),
                    );
                }
                source.set_onerror(None);
                source.close();
            }
        });
    }

    view! {
        <section class="live" aria-labelledby="operation-title">
            <h2 id="operation-title">"Live deployment"</h2>
            <p role="status" aria-live="polite">{move || status.get()}</p>
            {move || operation.get().map(|value| view! {
                <p class="muted">{format!("{} · generation {} · {} steps", value.kind, value.generation, value.steps.len())}</p>
            })}
            <LogControls logs=events/>
            <LogOutput logs=events label="Deployment event stream"/>
            {move || builds.get().into_iter().map(|build| view! { <BuildOutput build_view=build/> }).collect_view()}
        </section>
    }
}

#[component]
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
fn BuildOutput(build_view: BuildView) -> impl IntoView {
    let logs = create_rw_signal({
        let mut value = LogBuffer::default();
        value.follow = true;
        value
    });
    let status = create_rw_signal(build_view.state.clone());
    let sequence = create_rw_signal(0_u64);
    let alive = Rc::new(Cell::new(true));
    let client = Client::browser();
    {
        let client = client.clone();
        let id = build_view.id.clone();
        let alive = Rc::clone(&alive);
        spawn_local(async move {
            while alive.get() {
                match client.build(&id).await {
                    Ok(value) => status.set(value.state),
                    Err(error) => status.set(format!(
                        "build status unavailable: {}",
                        client_error_message(&error)
                    )),
                }
                match client.build_logs(&id, sequence.get_untracked(), 200).await {
                    Ok(page) => {
                        logs.update(|buffer| {
                            for entry in &page.items {
                                buffer.push(format!("{}: {}", entry.timestamp_ms, entry.message));
                            }
                        });
                        if let Some(last) = page.items.last() {
                            sequence.set(last.sequence);
                        }
                    }
                    Err(error) => status.set(format!(
                        "build output unavailable: {}",
                        client_error_message(&error)
                    )),
                }
                if matches!(
                    status.get_untracked().as_str(),
                    "succeeded" | "failed" | "cancelled"
                ) {
                    break;
                }
                TimeoutFuture::new(2_000).await;
            }
        });
    }
    on_cleanup(move || alive.set(false));
    view! {
        <section aria-label="Build output">
            <h3>{format!("Build {} · {}", build_view.service_name, build_view.id)}</h3>
            <p role="status" aria-live="polite">{move || status.get()}</p>
            <LogControls logs=logs/>
            <LogOutput logs=logs label="Bounded build log output"/>
        </section>
    }
}

#[component]
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
fn ApplicationObservability(application_id: String) -> impl IntoView {
    let status = create_rw_signal(None::<ApplicationStatusView>);
    let status_error = create_rw_signal(None::<String>);
    let logs = create_rw_signal({
        let mut value = LogBuffer::default();
        value.follow = true;
        value
    });
    let announcement = create_rw_signal("Connecting to runtime logs…".to_owned());
    let reconnect = create_rw_signal(ReconnectState::default());
    let client = Client::browser();
    let refresh = {
        let client = client.clone();
        let id = application_id.clone();
        move || {
            let client = client.clone();
            let id = id.clone();
            spawn_local(async move {
                match client.application_status(&id).await {
                    Ok(value) => {
                        status.set(Some(value));
                        status_error.set(None);
                    }
                    Err(error) => status_error.set(Some(client_error_message(&error))),
                }
                match client
                    .application_logs(
                        &id,
                        &ApplicationLogsOptions {
                            since_seconds: Some(300),
                            tail: Some(200),
                            max_bytes: Some(262_144),
                        },
                    )
                    .await
                {
                    Ok(records) => {
                        logs.update(|buffer| append_log_records(buffer, &records));
                        announcement.set("Runtime log snapshot received".into());
                    }
                    Err(error) => announcement.set(format!(
                        "Runtime log polling failed: {}",
                        client_error_message(&error)
                    )),
                }
            });
        }
    };
    refresh();
    {
        let refresh = refresh.clone();
        let alive = Rc::new(Cell::new(true));
        let loop_alive = Rc::clone(&alive);
        spawn_local(async move {
            while loop_alive.get() {
                TimeoutFuture::new(5_000).await;
                if loop_alive.get() {
                    refresh();
                }
            }
        });
        on_cleanup(move || alive.set(false));
    }

    let source = EventSource::new(&format!(
        "{API_PREFIX}/applications/{application_id}/logs?follow=true&since_seconds=300&tail=200&max_bytes=262144"
    ))
    .ok();
    let mut listeners = None;
    if let Some(source_ref) = source.as_ref() {
        let on_log = {
            Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                reconnect.update(|state| {
                    state.connected();
                    let id = event.last_event_id();
                    state.observed((!id.is_empty()).then_some(id));
                });
                if let Some(records) = parse_event::<Vec<ContainerLogView>>(&event) {
                    logs.update(|buffer| append_log_records(buffer, &records));
                    announcement.set("Runtime log received".into());
                }
            })
        };
        let _ =
            source_ref.add_event_listener_with_callback("logs", on_log.as_ref().unchecked_ref());
        let on_error = {
            Closure::<dyn FnMut()>::new(move || {
                reconnect.update(|state| {
                    let message = if state.disconnected().is_some() {
                        "Runtime stream interrupted; reconnecting…"
                    } else {
                        "Runtime stream paused; polling fallback is active."
                    };
                    announcement.set(message.into());
                });
            })
        };
        source_ref.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        listeners = Some((on_log, on_error));
    }
    on_cleanup(move || {
        if let Some(source) = source {
            if let Some((on_log, _on_error)) = listeners {
                let _ = source
                    .remove_event_listener_with_callback("logs", on_log.as_ref().unchecked_ref());
            }
            source.set_onerror(None);
            source.close();
        }
    });

    let export = {
        let client = client.clone();
        let id = application_id.clone();
        move || {
            let client = client.clone();
            let id = id.clone();
            spawn_local(async move {
                match client.export_application(&id, true).await {
                    Ok(document) => download_bytes(
                        document.as_bytes(),
                        "application/toml",
                        "piqueld-application.toml",
                    ),
                    Err(error) => announcement.set(format!(
                        "Application export failed: {}",
                        client_error_message(&error)
                    )),
                }
            });
        }
    };

    view! {
        <section class="live" aria-labelledby="runtime-title">
            <div class="page-heading"><div><p class="eyebrow">"Observed state"</p><h2 id="runtime-title">"Status and runtime logs"</h2></div><div class="actions"><button class="button button-secondary" type="button" on:click=move |_| refresh()>"Refresh status"</button><button class="button button-secondary" type="button" on:click=move |_| export()>"Export application"</button></div></div>
            {move || status_error.get().map(|message| view! { <p class="notice error" role="alert">{message}</p> })}
            {move || status.get().map(|value| view! { <div class="report"><dl class="detail-facts"><div><dt>"State"</dt><dd>{value.state}</dd></div><div><dt>"Observed generation"</dt><dd>{value.observed_generation.map_or_else(|| "not observed".into(), |generation| generation.to_string())}</dd></div><div><dt>"Infrastructure"</dt><dd>{value.infrastructure.unwrap_or_else(|| "not used".into())}</dd></div></dl><p>{value.message.unwrap_or_else(|| "No additional daemon diagnostic.".into())}</p></div> })}
            <p class="visually-hidden" aria-live="polite">{move || announcement.get()}</p>
            <LogControls logs=logs/>
            <LogOutput logs=logs label="Bounded runtime log output"/>
        </section>
    }
}

#[component]
fn LogControls(logs: RwSignal<LogBuffer>) -> impl IntoView {
    view! {
        <div class="log-controls">
            <button class="button button-secondary" type="button" aria-pressed=move || logs.get().paused on:click=move |_| logs.update(|value| value.set_paused(!value.paused))>{move || if logs.get().paused { "Resume" } else { "Pause" }}</button>
            <button class="button button-secondary" type="button" aria-pressed=move || logs.get().follow on:click=move |_| logs.update(|value| value.follow = !value.follow)>{move || if logs.get().follow { "Following" } else { "Follow" }}</button>
            <span aria-live="polite">{move || format!("{} / {} lines", logs.get().lines().count(), MAX_LOG_LINES)}</span>
        </div>
    }
}

#[component]
fn LogOutput(logs: RwSignal<LogBuffer>, label: &'static str) -> impl IntoView {
    let output = create_node_ref::<html::Pre>();
    create_effect(move |_| {
        let value = logs.get();
        if value.follow
            && !value.paused
            && let Some(element) = output.get()
        {
            element.set_scroll_top(element.scroll_height());
        }
    });
    view! { <pre node_ref=output tabindex="0" aria-label=label>{move || logs.get().lines().collect::<Vec<_>>().join("\n")}</pre> }
}

fn load_secrets(
    client: Client,
    secrets: RwSignal<Vec<SecretMetadata>>,
    loading: RwSignal<bool>,
    notice: RwSignal<Notice>,
) {
    loading.set(true);
    spawn_local(async move {
        let mut cursor = None;
        let mut values = Vec::new();
        loop {
            match client
                .secrets_with(&ListSecretsOptions {
                    cursor: cursor.clone(),
                    limit: Some(PAGE_LIMIT),
                })
                .await
            {
                Ok(page) => {
                    values.extend(page.items);
                    cursor = page.next_cursor;
                    if cursor.is_none() || values.len() >= MAX_LOG_LINES {
                        break;
                    }
                }
                Err(error) => {
                    notice.set(Notice {
                        message: client_error_message(&error),
                        error: true,
                    });
                    loading.set(false);
                    return;
                }
            }
        }
        secrets.set(values);
        loading.set(false);
    });
}

#[component]
#[allow(clippy::too_many_lines)]
fn Secrets() -> impl IntoView {
    let secrets = create_rw_signal(Vec::<SecretMetadata>::new());
    let loading = create_rw_signal(true);
    let notice = create_rw_signal(Notice::default());
    let name = create_rw_signal(String::new());
    let busy = create_rw_signal(false);
    let secret_value = Rc::new(RefCell::new(SecretDraft::default()));
    let value_input = create_node_ref::<html::Input>();
    let client = Client::browser();
    load_secrets(client.clone(), secrets, loading, notice);

    let submit = {
        let client = client.clone();
        let secret_value = Rc::clone(&secret_value);
        move |replace: bool| {
            if busy.get_untracked() {
                return;
            }
            let secret_name = name.get_untracked();
            let mut bytes = secret_value.borrow().bytes().to_vec();
            if secret_name.trim().is_empty() || bytes.is_empty() {
                bytes.zeroize();
                notice.set(Notice {
                    message: "Name and value are required.".into(),
                    error: true,
                });
                return;
            }
            busy.set(true);
            let client = client.clone();
            let secret_value = Rc::clone(&secret_value);
            spawn_local(async move {
                let result = if replace {
                    client.replace_secret(&secret_name, bytes).await
                } else {
                    client.create_secret(&secret_name, bytes).await
                };
                match result {
                    Ok(_) => {
                        secret_value.borrow_mut().clear();
                        if let Some(input) = value_input.get() {
                            input.set_value("");
                        }
                        name.set(String::new());
                        notice.set(Notice {
                            message: "Secret submitted; the local value was cleared.".into(),
                            error: false,
                        });
                        load_secrets(client.clone(), secrets, loading, notice);
                    }
                    Err(error) => notice.set(Notice {
                        message: format!(
                            "Secret submission failed: {}",
                            client_error_message(&error)
                        ),
                        error: true,
                    }),
                }
                busy.set(false);
            });
        }
    };
    let create_submit = submit.clone();
    let replace_submit = submit;
    let cleanup_value = Rc::clone(&secret_value);
    on_cleanup(move || {
        cleanup_value.borrow_mut().clear();
        if let Some(input) = value_input.get() {
            input.set_value("");
        }
    });
    view! {
        <section class="page" aria-labelledby="secrets-title">
            <div class="page-heading"><div><p class="eyebrow">"Write-only values"</p><h1 id="secrets-title">"Secrets"</h1></div></div>
            <ErrorNotice notice=notice/>
            <section class="card secret-form">
                <h2>"Create or rotate"</h2>
                <p>"Values are sent once and cannot be revealed. They are not stored in browser storage and the field is cleared after a successful submission."</p>
                <form autocomplete="off" on:submit=|event| event.prevent_default()>
                    <label for="secret-name">"Logical name"</label>
                    <input id="secret-name" autocomplete="off" prop:value=move || name.get() on:input=move |event| name.set(event_target_value(&event))/>
                    <label for="secret-value">"New value"</label>
                    <input node_ref=value_input id="secret-value" type="password" autocomplete="new-password" data-lpignore="true" data-1p-ignore="true" on:input=move |event| { let mut value = event_target_value(&event); secret_value.borrow_mut().replace(value.as_bytes()); value.zeroize(); }/>
                    <div class="actions"><button class="button button-primary" type="button" disabled=move || busy.get() on:click=move |_| create_submit(false)>"Create"</button><button class="button button-secondary" type="button" disabled=move || busy.get() on:click=move |_| replace_submit(true)>"Replace / rotate"</button></div>
                </form>
            </section>
            {move || loading.get().then(|| view! { <p class="loading" role="status">"Loading secret metadata…"</p> })}
            <div class="app-grid">
                {move || secrets.get().into_iter().map(|secret| {
                    let secret_name = secret.name.clone();
                    view! {
                        <article class="card">
                            <h2>{secret.name}</h2>
                            <p>{format!("Generation {} · {} references · value {}", secret.generation, secret.references.len(), if secret.value_is_set { "set" } else { "missing" })}</p>
                            <button class="button button-secondary danger-text" type="button" disabled=move || busy.get() on:click=move |_| {
                                if busy.get_untracked() || !browser_window().is_some_and(|window| window.confirm_with_message("Delete this unreferenced secret metadata and value?").unwrap_or(false)) { return; }
                                busy.set(true);
                                let client = Client::browser();
                                let secret_name = secret_name.clone();
                                spawn_local(async move {
                                    match client.delete_secret(&secret_name).await {
                                        Ok(()) => { notice.set(Notice { message: "Secret deleted.".into(), error: false }); load_secrets(client.clone(), secrets, loading, notice); }
                                        Err(error) => notice.set(Notice { message: client_error_message(&error), error: true }),
                                    }
                                    busy.set(false);
                                });
                            }>{"Delete metadata and value"}</button>
                        </article>
                    }
                }).collect_view()}
            </div>
        </section>
    }
}

#[component]
fn StateTransfer() -> impl IntoView {
    const REPLACE_CONFIRMATION: &str = "REPLACE CONTROL-PLANE STATE";
    let notice = create_rw_signal(Notice::default());
    let confirmation = create_rw_signal(String::new());
    let report = create_rw_signal(None::<StateImportResult>);
    let busy = create_rw_signal(false);
    let file_input = create_node_ref::<html::Input>();
    let client = Client::browser();

    let export = {
        let client = client.clone();
        Rc::new(move |mode: StateExportMode, file_name: &'static str| {
            let client = client.clone();
            spawn_local(async move {
                match client.export_state(mode).await {
                    Ok(bytes) => {
                        download_bytes(&bytes, "application/vnd.piqueld.state-v1+tar", file_name);
                    }
                    Err(error) => notice.set(Notice {
                        message: client_error_message(&error),
                        error: true,
                    }),
                }
            });
        })
    };
    let import_client = client.clone();

    view! {
        <section class="page" aria-labelledby="state-title">
            <div class="page-heading"><div><p class="eyebrow">"Portable recovery"</p><h1 id="state-title">"State export and import"</h1></div></div>
            <ErrorNotice notice=notice/>
            <div class="grid two">
                <section class="card"><h2>"Export"</h2><p>"Portable export omits secret values. Encrypted export includes envelopes but never the master key."</p><div class="actions"><button class="button button-primary" type="button" on:click={let export = Rc::clone(&export); move |_| export(StateExportMode::Portable, "piqueld-state-v1.tar")}>"Download portable state"</button><button class="button button-secondary" type="button" on:click={let export = Rc::clone(&export); move |_| export(StateExportMode::Encrypted, "piqueld-state-v1-encrypted.tar")}>"Download encrypted state"</button></div></section>
                <section class="card">
                    <h2>"Replace state from archive"</h2>
                    <p class="warning">"Import replaces control-plane state transactionally and pauses mutations. Verify the dependency report before deploying."</p>
                    <label for="state-confirmation">"Type "<code>{REPLACE_CONFIRMATION}</code>" to enable replacement"</label>
                    <input id="state-confirmation" autocomplete="off" spellcheck="false" prop:value=move || confirmation.get() on:input=move |event| confirmation.set(event_target_value(&event))/>
                    <label for="state-file">"State archive"</label>
                    <input node_ref=file_input id="state-file" type="file" accept="application/vnd.piqueld.state-v1+tar,.tar" disabled=move || confirmation.get() != REPLACE_CONFIRMATION on:change=move |event| {
                        let Some(input) = event.target().and_then(|target| target.dyn_into::<HtmlInputElement>().ok()) else { return };
                        let Some(file) = input.files().and_then(|files| files.get(0)) else { return };
                        if file.size() <= 0.0 || file.size() > 32.0 * 1024.0 * 1024.0 {
                            input.set_value("");
                            notice.set(Notice { message: "The archive must be non-empty and no larger than 32 MiB.".into(), error: true });
                            return;
                        }
                        if confirmation.get_untracked() != REPLACE_CONFIRMATION {
                            input.set_value("");
                            notice.set(Notice { message: format!("Type {REPLACE_CONFIRMATION} exactly before selecting an archive."), error: true });
                            return;
                        }
                        busy.set(true);
                        let client = import_client.clone();
                        spawn_local(async move {
                            let result = async {
                                let buffer = wasm_bindgen_futures::JsFuture::from(file.array_buffer()).await.map_err(|_| "Could not read archive".to_owned())?;
                                let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
                                if bytes.is_empty() || bytes.len() > MAX_STATE_ARCHIVE_BYTES {
                                    return Err("The archive size is outside the accepted bound.".to_owned());
                                }
                                let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
                                let mut token = client.prepare_state_import(&digest).await.map_err(|error| client_error_message(&error))?.token;
                                let imported = client.import_state(bytes, &token).await.map_err(|error| client_error_message(&error));
                                token.zeroize();
                                imported
                            }.await;
                            match result {
                                Ok(value) => { report.set(Some(value)); confirmation.set(String::new()); notice.set(Notice { message: "State imported. Review the dependency report below.".into(), error: false }); }
                                Err(message) => notice.set(Notice { message, error: true }),
                            }
                            input.set_value("");
                            busy.set(false);
                        });
                    }/>
                    {move || busy.get().then(|| view! { <p role="status">"Preparing transactional replacement…"</p> })}
                    <p class="help">"The archive digest is confirmed immediately before replacement and the confirmation is single-use."</p>
                </section>
            </div>
            {move || report.get().map(|value| view! { <section class="report"><h2>"Dependency report"</h2><p>{format!("Imported {} applications and {} secrets; operation {}.", value.applications_imported, value.secrets_imported, value.operation_id)}</p><pre>{serde_json::to_string_pretty(&value.dependencies).unwrap_or_default()}</pre></section> })}
        </section>
    }
}

fn download_bytes(bytes: &[u8], content_type: &str, file_name: &str) {
    let array = js_sys::Uint8Array::from(bytes);
    let parts = js_sys::Array::new();
    parts.push(&array.buffer());
    let options = web_sys::BlobPropertyBag::new();
    options.set_type(content_type);
    let Ok(blob) = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &options) else {
        return;
    };
    let Ok(url) = Url::create_object_url_with_blob(&blob) else {
        return;
    };
    if let Some(document) = browser_window().and_then(|window| window.document())
        && let Ok(element) = document.create_element("a")
        && let Ok(anchor) = element.dyn_into::<web_sys::HtmlAnchorElement>()
    {
        anchor.set_href(&url);
        anchor.set_download(file_name);
        anchor.click();
    }
    let _ = Url::revoke_object_url(&url);
}
