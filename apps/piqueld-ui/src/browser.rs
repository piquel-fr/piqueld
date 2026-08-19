//! Leptos client-side-rendered dashboard components.

use crate::state::{
    ApplicationHealth, ConnectionState, DataState, MAX_PAGES, PAGE_LIMIT, PaginationState,
    PollController,
};
use gloo_timers::future::TimeoutFuture;
use leptos::{
    CollectView, IntoView, RwSignal, SignalGet, SignalGetUntracked, SignalSet, View, component,
    create_rw_signal, ev, mount_to_body, on_cleanup, spawn_local, view, window_event_listener,
};
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
    detail_error: RwSignal<Option<String>>,
}

/// Mounts the CSR application into the document body.
pub fn mount() {
    mount_to_body(|| view! { <Dashboard/> });
}

type Refresh = Rc<dyn Fn()>;
type SelectApplication = Rc<dyn Fn(String)>;

#[component]
fn Dashboard() -> impl IntoView {
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

    dashboard_view(signals, &refresh, select)
}

fn dashboard_view(signals: DashboardSignals, refresh: &Refresh, select: SelectApplication) -> View {
    view! {
        <a class="skip-link" href="#dashboard-main">"Skip to main content"</a>
        {dashboard_header(signals, Rc::clone(refresh))}

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

                <ApplicationDetail signals=signals/>
            </div>
        </main>
        <footer class="site-footer">"piqueld · loopback dashboard · API v1"</footer>
    }
    .into_view()
}

fn dashboard_header(signals: DashboardSignals, refresh: Refresh) -> View {
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
fn ApplicationDetail(signals: DashboardSignals) -> impl IntoView {
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
                    detail_view(&detail, signals)
                } else {
                    let message = signals.detail_error.get().unwrap_or_else(|| "Application detail is unavailable.".into());
                    view! { <div class="state-panel" role="alert"><h3>"Detail unavailable"</h3><p>{message}</p></div> }.into_view()
                }
            }}
        </section>
    }
    .into_view()
}

fn detail_view(detail: &ApplicationDetailView, signals: DashboardSignals) -> View {
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
                <div><h3>{application_name}</h3><p class="muted"><code>{application_id}</code></p></div>
                <span class=format!("health-badge {}", health_class(health))>{health.label()}</span>
            </div>
            <dl class="detail-facts">
                <div><dt>"Desired generation"</dt><dd>{detail.application.generation}</dd></div>
                <div><dt>"Observed generation"</dt><dd>{status.observed_generation.map_or_else(|| "Not observed".into(), |generation| generation.to_string())}</dd></div>
                <div><dt>"Networks / volumes"</dt><dd>{format!("{} / {}", observed.network_count, observed.volume_count)}</dd></div>
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
            <section class="subsection" aria-labelledby="diagnostic-title">
                <h3 id="diagnostic-title">"Reconciliation diagnostics"</h3>
                {if diagnostics.is_empty() {
                    view! { <p class="muted">"No diagnostics reported."</p> }.into_view()
                } else {
                    view! { <ul class="diagnostic-list">{diagnostics_view}</ul> }.into_view()
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
    if signals.detail_loading.get_untracked() {
        return;
    }
    signals.detail_loading.set(true);
    signals.detail_error.set(None);
    spawn_local(async move {
        match client.application_detail(&id).await {
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
