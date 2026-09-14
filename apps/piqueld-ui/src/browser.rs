//! Leptos client-side-rendered dashboard routes and shared data services.

mod dashboard;
mod management;
mod runtime;

use dashboard::{ApplicationsPage, OverviewPage, dashboard_header};

use crate::state::{
    ApplicationHealth, ConnectionState, DataState, MAX_PAGES, PAGE_LIMIT, PaginationState,
    PollController,
};
use futures_util::StreamExt;
use gloo_timers::future::TimeoutFuture;
use leptos::{
    IntoView, RwSignal, SignalGet, SignalGetUntracked, SignalSet, SignalWith, View, component,
    create_effect, create_rw_signal, ev, mount_to_body, on_cleanup, provide_context, spawn_local,
    view, window_event_listener,
};
use leptos_router::{A, Outlet, Redirect, Route, Router, Routes, TrailingSlash, use_params_map};
use piqueld_client::system::ReadinessStatus;
use piqueld_client::{
    ApplicationDetailView, ApplicationStatusView, ApplicationSummary, Client, ClientError,
    ListApplicationsOptions, Page, SystemStatus,
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use web_sys::window as browser_window;

#[derive(Clone, Debug)]
struct ApplicationRow {
    application: ApplicationSummary,
    status: Option<ApplicationStatusView>,
    status_error: Option<String>,
    deployments: Vec<piqueld_client::DeploymentView>,
    deployment_error: Option<String>,
}

#[derive(Clone, Debug)]
struct DashboardSnapshot {
    system: SystemStatus,
    readiness: Result<ReadinessStatus, String>,
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
    readiness: RwSignal<Option<ReadinessStatus>>,
    readiness_error: RwSignal<Option<String>>,
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
            readiness: create_rw_signal(None),
            readiness_error: create_rw_signal(None),
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
    mount_to_body(|| view! { <App /> });
}

#[component]
fn App() -> impl IntoView {
    management::HistoryGuard::install();
    view! {
        <Router trailing_slash={TrailingSlash::Exact} fallback={|| view! { <NotFoundPage /> }}>
            <Routes>
                <Route path="/" view={DashboardLayout}>
                    <Route path="/" view={OverviewPage} />
                    <Route path="/applications" view={ApplicationsPage} />
                    <Route path="/settings" view={management::HostPage} />
                    <Route path="/applications/:id" view={ApplicationDetailPage} />
                    <Route
                        path="/applications/:id/services/:service"
                        view={ApplicationDetailPage}
                    />
                </Route>
                <Route path="/dashboard" view={DashboardLayout}>
                    <Route path="" view={DashboardRedirect} />
                    <Route path="/applications" view={ApplicationsPage} />
                    <Route path="/settings" view={management::HostPage} />
                    <Route path="/applications/:id" view={ApplicationDetailPage} />
                    <Route
                        path="/applications/:id/services/:service"
                        view={ApplicationDetailPage}
                    />
                    <Route path="/*any" view={DashboardRouteFallback} />
                </Route>
                <Route path="/*any" view={NotFoundPage} />
            </Routes>
        </Router>
    }
}

#[component]
fn DashboardRedirect() -> impl IntoView {
    view! { <Redirect path="/dashboard/" /> }
}

#[component]
fn DashboardRouteFallback() -> impl IntoView {
    let params = use_params_map();
    let is_overview = params.with(|params| params.get("any").is_none_or(String::is_empty));

    if is_overview {
        view! { <OverviewPage /> }.into_view()
    } else {
        view! { <NotFoundPage /> }.into_view()
    }
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
    let active = Rc::new(Cell::new(true));
    spawn_poll_loop(client, signals, controller, Rc::clone(&active));
    on_cleanup(move || active.set(false));

    view! {
        <a class="skip-link" href="#dashboard-main">
            "Skip to main content"
        </a>
        {dashboard_header()}

        <main id="dashboard-main" class="dashboard-main" tabindex="-1">
            {refresh_error(&context)}
            {stale_notice(signals)}

            <Outlet />
        </main>
    }
}

fn dashboard_context() -> DashboardContext {
    leptos::use_context().expect("dashboard routes are descendants of DashboardLayout")
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
        <leptos::For
            each={move || {
                params
                    .with(|p| p.get("id").cloned().map(|id| (id, p.get("service").cloned())))
                    .into_iter()
                    .collect::<Vec<_>>()
            }}
            key={|route| route.clone()}
            children={move |(id, service)| {
                view! { <management::ApplicationPage id={id} service={service} /> }
            }}
        />
    }
}

#[component]
fn NotFoundPage() -> impl IntoView {
    view! {
        <section
            class="rounded-xl border border-line bg-surface p-6 shadow-panel"
            aria-labelledby="not-found-title"
        >
            <p class="mb-1 text-xs font-extrabold tracking-[.12em] text-accent">"NOT FOUND"</p>
            <h2 id="not-found-title" class="mb-2 text-2xl font-bold">
                "Dashboard page not found"
            </h2>
            <p class="mb-4 text-muted">"Choose a known dashboard destination to continue."</p>
            <A
                class="rounded-md border border-line bg-surface px-3 py-2 font-bold text-accent-strong hover:border-accent"
                href="/dashboard/"
            >
                "Return to overview"
            </A>
        </section>
    }
}

fn refresh_error(context: &DashboardContext) -> View {
    let signals = context.signals;
    view! {
        {move || {
            signals
                .refresh_error
                .get()
                .map(|message| {
                    view! {
                        <div
                            class="mb-4 rounded-xl border border-line border-l-4 border-l-bad bg-surface p-4 shadow-panel"
                            role="alert"
                            aria-live="assertive"
                        >
                            <h2 class="mb-1 text-lg font-bold">
                                {if signals.connection.get() == ConnectionState::Unreachable {
                                    "Daemon unreachable"
                                } else {
                                    "Refresh failed"
                                }}
                            </h2>
                            <p class="mb-2">{message}</p>
                        </div>
                    }
                })
        }}
    }
    .into_view()
}

fn stale_notice(signals: DashboardSignals) -> View {
    view! {
        {move || {
            (signals.data_state.get() == DataState::Stale)
                .then(|| {
                    view! {
                        <p
                            class="mb-4 rounded-lg border border-warn bg-warn-bg p-3 text-warn"
                            role="status"
                        >
                            "Showing the last successful view; the latest refresh failed."
                        </p>
                    }
                })
        }}
    }
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
        let result = fetch_snapshot(&client).await;
        if signals.system.try_get_untracked().is_none() {
            return;
        }
        match result {
            Ok(snapshot) => {
                controller.borrow_mut().record_success();
                signals.system.set(Some(snapshot.system));
                match snapshot.readiness {
                    Ok(readiness) => {
                        signals.readiness.set(Some(readiness));
                        signals.readiness_error.set(None);
                    }
                    Err(error) => {
                        signals.readiness.set(None);
                        signals.readiness_error.set(Some(error));
                    }
                }
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
                        .any(|row| row.application.id.to_string() == id)
                    {
                        load_detail(client.clone(), signals, id);
                    } else if !signals.pagination_incomplete.get_untracked() {
                        signals.selected_id.set(None);
                        signals.detail.set(None);
                        signals.detail_loading.set(false);
                    }
                }
            }
            Err(failure) => {
                controller.borrow_mut().record_failure();
                signals.readiness.set(None);
                signals.readiness_error.set(None);
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
        if controller.borrow().manual_pending() {
            start_refresh(client, signals, Rc::clone(&controller), false);
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
        if signals.detail_request.try_get_untracked() != Some(request)
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
    active: Rc<Cell<bool>>,
) {
    spawn_local(async move {
        while active.get() {
            let delay = controller.borrow().delay();
            let milliseconds = u32::try_from(delay.as_millis()).unwrap_or(u32::MAX);
            TimeoutFuture::new(milliseconds).await;
            if !active.get() {
                return;
            }
            start_refresh(client.clone(), signals, Rc::clone(&controller), false);
        }
    });
}

async fn fetch_snapshot(client: &Client) -> Result<DashboardSnapshot, LoadFailure> {
    let system = client
        .system_status()
        .await
        .map_err(|error| load_failure(&error))?;
    let readiness = client
        .system_readiness()
        .await
        .map_err(|error| client_error_message(&error));
    let mut pagination = PaginationState::new();
    let mut cursor = None;
    let mut applications = Vec::new();
    loop {
        let page: Page<ApplicationSummary> = client
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
                let id = application.id.to_string();
                let (status, status_error) = match client.application_status(&id).await {
                    Ok(status) => (Some(status), None),
                    Err(error) => (None, Some(client_error_message(&error))),
                };
                let (deployments, deployment_error) = match client.deployments(&id, None).await {
                    Ok(page) => (page.items, None),
                    Err(error) => (Vec::new(), Some(client_error_message(&error))),
                };
                ApplicationRow {
                    application,
                    status,
                    status_error,
                    deployments,
                    deployment_error,
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
        readiness,
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
        ClientError::Decode { .. } => "The daemon returned an invalid public API response.".into(),
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
                    ApplicationHealth::from_server_state(status.state)
                })
        },
        |_| ApplicationHealth::Pending,
    )
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
        ApplicationHealth::Pending | ApplicationHealth::NotDeployed => {
            "inline-flex w-fit items-center rounded-full bg-pending-bg px-2 py-1 text-xs font-extrabold text-pending"
        }
    }
}
