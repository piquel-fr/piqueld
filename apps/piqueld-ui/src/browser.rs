//! Leptos client-side-rendered dashboard routes and shared data services.

mod access;
mod auth;
mod builds;
mod dashboard;
mod format;
mod logs;
mod management;
mod observability;
mod runtime;
mod ui;

use dashboard::{ApplicationsPage, OverviewPage, Sidebar};
use ui::{Icon, Tone, icon, notice};

use crate::state::{
    ApplicationHealth, ConnectionState, DataState, MAX_PAGES, PAGE_LIMIT, PaginationState,
    PollController,
};
use futures_util::StreamExt;
use gloo_timers::future::TimeoutFuture;
use leptos::ev;
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::{A, Outlet, ParentRoute, Route, Router, Routes};
use leptos_router::hooks::use_params_map;
use leptos_router::path;
use piqueld_client::system::ReadinessStatus;
use piqueld_client::{
    ApplicationSummary, Client, ClientError, EnvironmentDetailView, EnvironmentStatusView,
    EnvironmentView, ListApplicationsOptions, Page, SystemStatus,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use web_sys::window as browser_window;

/// One application in the dashboard list, with its environments' statuses and
/// deployment history fetched alongside; per-application failures are kept as
/// messages so one broken application does not fail the whole refresh.
#[derive(Clone, Debug)]
struct ApplicationRow {
    application: ApplicationSummary,
    environments: Vec<EnvironmentRow>,
    /// Deployments of every environment.
    deployments: Vec<piqueld_client::DeploymentView>,
    deployment_error: Option<String>,
}

/// One environment of a listed application with its status, or the reason it
/// could not be read.
#[derive(Clone, Debug)]
struct EnvironmentRow {
    environment: EnvironmentView,
    status: Option<EnvironmentStatusView>,
    status_error: Option<String>,
}

impl EnvironmentRow {
    /// Health from the environment's status; missing or failed reads show as pending.
    fn health(&self) -> ApplicationHealth {
        match (&self.status, &self.status_error) {
            (Some(status), None) => ApplicationHealth::from_server_state(status.state),
            _ => ApplicationHealth::Pending,
        }
    }
}

impl ApplicationRow {
    /// The first status message among the application's environments.
    fn message(&self) -> Option<String> {
        self.environments.iter().find_map(|row| {
            row.status
                .as_ref()
                .and_then(|status| status.message.clone())
                .map(|message| format!("{}: {message}", row.environment.name))
        })
    }
}

/// Everything one successful refresh loaded, applied to `DashboardSignals` at once.
#[derive(Clone, Debug)]
struct DashboardSnapshot {
    system: SystemStatus,
    readiness: Result<ReadinessStatus, String>,
    applications: Vec<ApplicationRow>,
    incomplete: bool,
}

/// A refresh failure, split by whether the daemon was reachable at all.
#[derive(Clone, Debug)]
struct LoadFailure {
    unreachable: bool,
    message: String,
}

/// Reactive state shared by every dashboard route, written by the refresh loop.
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
    /// Environment page being shown, from the route; `None` on application pages.
    selected_environment: RwSignal<Option<String>>,
    detail: RwSignal<Option<EnvironmentDetailView>>,
    detail_loading: RwSignal<bool>,
    detail_request: RwSignal<u64>,
    detail_error: RwSignal<Option<String>>,
}

impl DashboardSignals {
    /// Creates every signal in its initial loading state.
    fn new() -> Self {
        Self {
            system: RwSignal::new(None),
            applications: RwSignal::new(Vec::new()),
            connection: RwSignal::new(ConnectionState::Loading),
            data_state: RwSignal::new(DataState::Loading),
            refresh_error: RwSignal::new(None),
            refreshing: RwSignal::new(false),
            readiness: RwSignal::new(None),
            readiness_error: RwSignal::new(None),
            pagination_incomplete: RwSignal::new(false),
            selected_id: RwSignal::new(None),
            selected_environment: RwSignal::new(None),
            detail: RwSignal::new(None),
            detail_loading: RwSignal::new(false),
            detail_request: RwSignal::new(0),
            detail_error: RwSignal::new(None),
        }
    }
}

/// Liveness flag for async work spawned by a component: the component's
/// reactive owner clears it on cleanup, so polling loops and late responses
/// stop before touching its disposed signals.
#[derive(Clone)]
struct Alive(Arc<AtomicBool>);

impl Alive {
    /// Creates a flag that the current reactive owner clears on cleanup.
    fn new() -> Self {
        let flag = Arc::new(AtomicBool::new(true));
        let cleanup = Arc::clone(&flag);
        on_cleanup(move || cleanup.store(false, Ordering::Relaxed));
        Self(flag)
    }

    /// Whether the owning component is still mounted.
    fn get(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Context provided by `DashboardLayout` to all nested dashboard routes.
#[derive(Clone)]
struct DashboardContext {
    signals: DashboardSignals,
    client: Client,
    /// Requests an immediate (manual) dashboard refresh.
    refresh: Callback<()>,
}

/// Mounts the CSR application into the document body.
pub fn mount() {
    mount_to_body(|| view! { <auth::Gate /> });
}

/// Root router. Dashboard pages live under `/dashboard` (the daemon redirects
/// `/` there), behind `auth::ProtectedDashboardLayout`; unknown dashboard paths
/// render `NotFoundPage` inside the layout. Also installs log preferences and
/// the unsaved-changes history guard.
#[component]
fn App() -> impl IntoView {
    logs::LogPreferences::provide();
    management::HistoryGuard::install();
    view! {
        <Router>
            <Routes fallback={|| view! { <NotFoundPage /> }}>
                <Route path={path!("/dashboard/auth")} view={auth::AuthPage} />
                <ParentRoute path={path!("/dashboard")} view={auth::ProtectedDashboardLayout}>
                    <Route path={path!("")} view={OverviewPage} />
                    <Route path={path!("applications")} view={ApplicationsPage} />
                    <Route path={path!("settings")} view={management::HostPage} />
                    <Route path={path!("accounts")} view={auth::AccountsPage} />
                    <Route path={path!("builds")} view={builds::BuildsPage} />
                    <Route path={path!("events")} view={observability::HistoryPage} />
                    <Route path={path!("errors")} view={observability::ErrorsPage} />
                    <Route path={path!("errors/:id")} view={observability::DiagnosticPage} />
                    <Route path={path!("system")} view={observability::SystemPage} />
                    <Route path={path!("analytics")} view={observability::AnalyticsPage} />
                    <Route path={path!("notifications")} view={observability::NotificationsPage} />
                    <Route path={path!("applications/:id")} view={ApplicationDetailPage} />
                    <Route
                        path={path!("applications/:id/services/:service")}
                        view={ApplicationDetailPage}
                    />
                    <Route
                        path={path!("applications/:id/environments/:environment")}
                        view={ApplicationDetailPage}
                    />
                    <Route path={path!("*any")} view={NotFoundPage} />
                </ParentRoute>
            </Routes>
        </Router>
    }
}

/// Shared shell for authenticated dashboard pages. Provides `DashboardContext`,
/// starts an immediate refresh plus a background poll loop (paused while the tab
/// is hidden), and renders the sidebar, refresh error, stale notice and the routed page.
#[component]
fn DashboardLayout() -> impl IntoView {
    let signals = DashboardSignals::new();
    let client = Client::browser();
    let mut poll = PollController::new();
    poll.set_hidden(document_hidden());
    let controller = StoredValue::new(poll);

    let refresh = {
        let client = client.clone();
        Callback::new(move |()| start_refresh(client.clone(), signals, controller, true))
    };
    let context = DashboardContext {
        signals,
        client: client.clone(),
        refresh,
    };
    provide_context(context.clone());

    let visibility_listener = window_event_listener(ev::visibilitychange, move |_| {
        controller.update_value(|controller| controller.set_hidden(document_hidden()));
    });
    on_cleanup(move || visibility_listener.remove());

    start_refresh(client.clone(), signals, controller, true);
    spawn_poll_loop(client, signals, controller, Alive::new());

    view! {
        <a class="skip-link" href="#dashboard-main">
            "Skip to main content"
        </a>
        <Sidebar />

        <main id="dashboard-main" class="dashboard-main" tabindex="-1">
            {refresh_error(&context)}
            {stale_notice(signals)}

            <Outlet />
        </main>
    }
}

/// Returns the context provided by `DashboardLayout`.
///
/// Panics when called outside a dashboard route.
fn dashboard_context() -> DashboardContext {
    use_context().expect("dashboard routes are descendants of DashboardLayout")
}

/// Route wrapper for `/applications/:id[/services/:service|/environments/:environment]`.
/// Loads the shown environment's detail into the shared signals when the route
/// changes, and keys `management::ApplicationPage` on the route so it remounts
/// on navigation.
#[component]
fn ApplicationDetailPage() -> impl IntoView {
    let context = dashboard_context();
    let signals = context.signals;
    let params = use_params_map();
    let client = context.client.clone();

    Effect::new(move |_| {
        let (id, environment) = params.with(|params| (params.get("id"), params.get("environment")));
        let Some(id) = id else {
            return;
        };
        if signals.selected_id.get_untracked().as_deref() == Some(id.as_str())
            && signals.detail.get_untracked().is_some()
            && environment == signals.selected_environment.get_untracked()
        {
            return;
        }
        signals.selected_id.set(Some(id.clone()));
        signals.selected_environment.set(environment);
        signals.detail.set(None);
        load_detail(client.clone(), signals, id);
    });

    view! {
        <For
            each={move || {
                params
                    .with(|p| {
                        p.get("id")
                            .map(|id| {
                                let page = match (p.get("service"), p.get("environment")) {
                                    (Some(service), _) => management::Page::Service(service),
                                    (None, Some(environment)) => {
                                        management::Page::Environment(environment)
                                    }
                                    (None, None) => management::Page::Application,
                                };
                                (id, page)
                            })
                    })
                    .into_iter()
                    .collect::<Vec<_>>()
            }}
            key={|route| route.clone()}
            children={move |(id, page)| {
                view! { <management::ApplicationPage id={id} page={page} /> }
            }}
        />
    }
}

/// Static "page not found" panel with a link back to the overview.
#[component]
fn NotFoundPage() -> impl IntoView {
    view! {
        <section class="card" aria-labelledby="not-found-title">
            <h2 id="not-found-title">"Page not found"</h2>
            <p class="hint">"This dashboard address does not exist."</p>
            <div class="form-actions">
                <A attr:class="btn" href="/dashboard/">
                    {icon(Icon::ArrowLeft)}
                    "Back to overview"
                </A>
            </div>
        </section>
    }
}

/// Alert shown while the last refresh failed, titled by whether the daemon was unreachable.
fn refresh_error(context: &DashboardContext) -> AnyView {
    let signals = context.signals;
    view! {
        <div
            class="stack-sm"
            style="margin-bottom:16px"
            hidden={move || signals.refresh_error.get().is_none()}
        >
            {move || {
                signals
                    .refresh_error
                    .get()
                    .map(|message| {
                        let title = if signals.connection.get() == ConnectionState::Unreachable {
                            "Daemon unreachable"
                        } else {
                            "Refresh failed"
                        };
                        notice(
                            Tone::Bad,
                            view! {
                                <strong>{title}</strong>
                                {message}
                            },
                        )
                    })
            }}
        </div>
    }
    .into_any()
}

/// Notice shown when the dashboard is displaying data from an earlier refresh.
fn stale_notice(signals: DashboardSignals) -> AnyView {
    view! {
        <div
            class="stack-sm"
            style="margin-bottom:16px"
            hidden={move || signals.data_state.get() != DataState::Stale}
        >
            {notice(Tone::Warn, "Showing the last successful view; the latest refresh failed.")}
        </div>
    }
    .into_any()
}

/// Starts one dashboard refresh if `PollController` grants the single in-flight slot.
/// `manual` refreshes are queued even while hidden or busy and rerun once the current
/// request finishes. On success all dashboard signals are replaced and the selected
/// application's detail is reloaded, or cleared if it disappeared from a complete
/// listing. On failure application data is kept (marked stale unless still loading)
/// and readiness is cleared.
fn start_refresh(
    client: Client,
    signals: DashboardSignals,
    controller: StoredValue<PollController>,
    manual: bool,
) {
    if manual {
        controller.update_value(PollController::request_manual_refresh);
    }
    if !controller
        .try_update_value(PollController::begin_request)
        .unwrap_or(false)
    {
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
                controller.update_value(PollController::record_success);
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
                controller.update_value(PollController::record_failure);
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
        if controller.with_value(PollController::manual_pending) {
            start_refresh(client, signals, controller, false);
        }
    });
}

/// Fetches detail for the shown environment of application `id`: the route's
/// environment, or on application pages its only environment. Ignores the
/// response if a newer request started or the selection changed meanwhile.
fn load_detail(client: Client, signals: DashboardSignals, id: String) {
    let request = signals.detail_request.get_untracked().wrapping_add(1);
    signals.detail_request.set(request);
    signals.detail_loading.set(true);
    signals.detail_error.set(None);
    let selected = signals.selected_environment.get_untracked();
    spawn_local(async move {
        let result = environment_detail(&client, &id, selected.as_deref()).await;
        if signals.detail_request.try_get_untracked() != Some(request)
            || signals.selected_id.get_untracked().as_deref() != Some(id.as_str())
        {
            return;
        }
        match result {
            Ok(detail) => signals.detail.set(detail),
            Err(error) => signals.detail_error.set(Some(error)),
        }
        signals.detail_loading.set(false);
    });
}

/// Loads the application, then the environment with the selected ID or, with
/// no selection, its only environment. Applications with several environments
/// (or none) have no environment detail of their own.
async fn environment_detail(
    client: &Client,
    id: &str,
    selected: Option<&str>,
) -> Result<Option<EnvironmentDetailView>, String> {
    let application = client
        .application(id)
        .await
        .map_err(|error| client_error_message(&error))?;
    // A selection that no longer exists is reported instead of silently
    // replaced, so later actions never target another environment.
    let environment = match selected {
        Some(selected) => application
            .environments
            .iter()
            .find(|environment| environment.id.as_str() == selected)
            .ok_or_else(|| "This environment no longer exists.".to_owned())?,
        None => match application.sole_environment() {
            Ok(environment) => environment,
            Err(_) => return Ok(None),
        },
    };
    client
        .environment_detail(environment.id.as_str())
        .await
        .map(Some)
        .map_err(|error| client_error_message(&error))
}

/// Background loop that triggers a refresh after each `PollController` delay
/// until the layout unmounts.
fn spawn_poll_loop(
    client: Client,
    signals: DashboardSignals,
    controller: StoredValue<PollController>,
    active: Alive,
) {
    spawn_local(async move {
        while active.get() {
            let delay = controller.with_value(PollController::delay);
            let milliseconds = u32::try_from(delay.as_millis()).unwrap_or(u32::MAX);
            TimeoutFuture::new(milliseconds).await;
            if !active.get() {
                return;
            }
            start_refresh(client.clone(), signals, controller, false);
        }
    });
}

/// Loads system status, readiness and every application page (bounded by
/// `MAX_PAGES`), fetching each application's environment statuses and
/// deployments concurrently.
/// Only system status and listing failures abort; readiness and per-application
/// errors are recorded in the snapshot.
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
        let page: Page<ApplicationSummary> = match client
            .applications_with(&ListApplicationsOptions {
                cursor: cursor.clone(),
                limit: Some(PAGE_LIMIT),
            })
            .await
        {
            Ok(page) => page,
            // Accounts without application access see an empty directory.
            Err(piqueld_client::ClientError::Api { status, .. }) if status.as_u16() == 403 => {
                Page {
                    items: Vec::new(),
                    next_cursor: None,
                }
            }
            Err(error) => return Err(load_failure(&error)),
        };
        let next_cursor = page.next_cursor.clone();
        // Status reads are independent, so a bounded pool keeps one slow
        // application from serializing the whole refresh.
        let statuses = futures_util::stream::iter(page.items.into_iter().map(|application| {
            let client = client.clone();
            async move {
                let mut environments = Vec::with_capacity(application.environments.len());
                let mut deployments = Vec::new();
                let mut deployment_error = None;
                for environment in &application.environments {
                    let id = environment.id.as_str();
                    let (status, status_error) = match client.environment_status(id).await {
                        Ok(status) => (Some(status), None),
                        Err(error) => (None, Some(client_error_message(&error))),
                    };
                    match client.deployments(id, None).await {
                        Ok(page) => deployments.extend(page.items),
                        Err(error) => deployment_error = Some(client_error_message(&error)),
                    }
                    environments.push(EnvironmentRow {
                        environment: environment.clone(),
                        status,
                        status_error,
                    });
                }
                ApplicationRow {
                    application,
                    environments,
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

/// Classifies a client error as unreachable (transport) or failed.
fn load_failure(error: &ClientError) -> LoadFailure {
    LoadFailure {
        unreachable: matches!(error, ClientError::Transport { .. }),
        message: client_error_message(error),
    }
}

/// User-facing message for a client error.
fn client_error_message(error: &ClientError) -> String {
    match error {
        ClientError::Endpoint { message } => {
            format!("The dashboard endpoint is invalid: {message}")
        }
        ClientError::Transport { message, .. } => format!("Could not reach piqueld: {message}"),
        ClientError::Api { error, .. } => error.message.clone(),
        ClientError::Decode { .. } | ClientError::TextDecode { .. } => {
            "The daemon returned an invalid public API response.".into()
        }
    }
}

/// Returns whether the browser tab is currently hidden.
fn document_hidden() -> bool {
    browser_window()
        .and_then(|window| window.document())
        .is_some_and(|document| document.hidden())
}

/// Header label for a connection state.
fn connection_label(state: ConnectionState) -> &'static str {
    match state {
        ConnectionState::Loading => "Checking…",
        ConnectionState::Reachable => "Reachable",
        ConnectionState::Failed => "Request failed",
        ConnectionState::Unreachable => "Unreachable",
    }
}

/// Health badge for a row: the least healthy of its environments, or not
/// deployed without any.
fn row_health(row: &ApplicationRow) -> ApplicationHealth {
    row.environments
        .iter()
        .map(EnvironmentRow::health)
        .max_by_key(|health| health.severity())
        .unwrap_or(ApplicationHealth::NotDeployed)
}

/// The listed row of an environment, for its health and status.
fn environment_row(signals: DashboardSignals, environment: &str) -> Option<EnvironmentRow> {
    signals.applications.with(|rows| {
        rows.iter()
            .flat_map(|row| &row.environments)
            .find(|row| row.environment.id.as_str() == environment)
            .cloned()
    })
}

/// The application and dashboard address of an environment in the current
/// listing, or `None` when it is not listed (for example after deletion).
fn environment_link(signals: DashboardSignals, environment: &str) -> Option<(String, String)> {
    signals.applications.with(|rows| {
        rows.iter().find_map(|row| {
            row.environments
                .iter()
                .find(|listed| listed.environment.id.as_str() == environment)
                .map(|listed| {
                    (
                        format!("{}/{}", row.application.name, listed.environment.name),
                        format!(
                            "/dashboard/applications/{}/environments/{environment}",
                            row.application.id
                        ),
                    )
                })
        })
    })
}
