//! Leptos client-side-rendered dashboard routes and shared data services.

mod access;
mod auth;
mod builds;
mod dashboard;
mod format;
mod logs;
mod management;
mod notifications;
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
    EnvironmentView, ListApplicationsOptions, SystemStatus,
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
    /// Health badge for a row: the least healthy of its environments, or not
    /// deployed without any.
    fn health(&self) -> ApplicationHealth {
        self.environments
            .iter()
            .map(EnvironmentRow::health)
            .max_by_key(|health| health.severity())
            .unwrap_or(ApplicationHealth::NotDeployed)
    }

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

impl From<ClientError> for LoadFailure {
    fn from(error: ClientError) -> Self {
        Self {
            unreachable: error.transport(),
            message: error.user_message(),
        }
    }
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

    /// Starts one dashboard refresh if `PollController` grants the single in-flight slot.
    /// `manual` refreshes are queued even while hidden or busy and rerun once the current
    /// request finishes. On success all dashboard signals are replaced and the selected
    /// application's detail is reloaded, or cleared if it disappeared from a complete
    /// listing. On failure application data is kept (marked stale unless still loading)
    /// and readiness is cleared.
    fn start_refresh(self, client: Client, controller: StoredValue<PollController>, manual: bool) {
        if manual {
            controller.update_value(PollController::request_manual_refresh);
        }
        if !controller
            .try_update_value(PollController::begin_request)
            .unwrap_or(false)
        {
            return;
        }
        self.refreshing.set(true);
        spawn_local(async move {
            let result = DashboardSnapshot::fetch(&client).await;
            // The layout unmounted meanwhile and disposed its signals.
            if self.system.try_get_untracked().is_none() {
                return;
            }
            match result {
                Ok(snapshot) => {
                    controller.update_value(PollController::record_success);
                    self.apply(snapshot, &client);
                }
                Err(failure) => {
                    controller.update_value(PollController::record_failure);
                    self.readiness.set(None);
                    self.readiness_error.set(None);
                    self.connection.set(if failure.unreachable {
                        ConnectionState::Unreachable
                    } else {
                        ConnectionState::Failed
                    });
                    if self.data_state.get_untracked() != DataState::Loading {
                        self.data_state.set(DataState::Stale);
                    }
                    self.refresh_error.set(Some(failure.message));
                    self.refreshing.set(false);
                }
            }
            if controller.with_value(PollController::manual_pending) {
                self.start_refresh(client, controller, false);
            }
        });
    }

    /// Replaces every signal with a successful refresh, then reloads the
    /// selected application's detail, or clears it if it disappeared from a
    /// complete listing.
    fn apply(self, snapshot: DashboardSnapshot, client: &Client) {
        self.system.set(Some(snapshot.system));
        let (readiness, readiness_error) = match snapshot.readiness {
            Ok(readiness) => (Some(readiness), None),
            Err(error) => (None, Some(error)),
        };
        self.readiness.set(readiness);
        self.readiness_error.set(readiness_error);
        let data_state = if snapshot.applications.is_empty() {
            DataState::Empty
        } else {
            DataState::Ready
        };
        self.applications.set(snapshot.applications);
        self.pagination_incomplete.set(snapshot.incomplete);
        self.connection.set(ConnectionState::Reachable);
        self.data_state.set(data_state);
        self.refresh_error.set(None);
        self.refreshing.set(false);
        if let Some(id) = self.selected_id.get_untracked() {
            if self
                .applications
                .with_untracked(|rows| rows.iter().any(|row| row.application.id.as_str() == id))
            {
                self.load_detail(client.clone(), id);
            } else if !snapshot.incomplete {
                self.selected_id.set(None);
                self.detail.set(None);
                self.detail_loading.set(false);
            }
        }
    }

    /// Background loop that triggers a refresh after each `PollController` delay
    /// until the layout unmounts.
    fn spawn_poll_loop(self, client: Client, controller: StoredValue<PollController>) {
        let active = Alive::new();
        spawn_local(async move {
            while active.get() {
                let delay = controller.with_value(PollController::delay);
                let milliseconds = u32::try_from(delay.as_millis()).unwrap_or(u32::MAX);
                TimeoutFuture::new(milliseconds).await;
                if !active.get() {
                    return;
                }
                self.start_refresh(client.clone(), controller, false);
            }
        });
    }

    /// Fetches detail for the shown environment of application `id`: the route's
    /// environment, or on application pages its only environment. Ignores the
    /// response if a newer request started or the selection changed meanwhile.
    fn load_detail(self, client: Client, id: String) {
        let request = self.detail_request.get_untracked().wrapping_add(1);
        self.detail_request.set(request);
        self.detail_loading.set(true);
        self.detail_error.set(None);
        let selected = self.selected_environment.get_untracked();
        spawn_local(async move {
            let result = environment_detail(&client, &id, selected.as_deref()).await;
            if self.detail_request.try_get_untracked() != Some(request)
                || self.selected_id.get_untracked().as_deref() != Some(id.as_str())
            {
                return;
            }
            match result {
                Ok(detail) => self.detail.set(detail),
                Err(error) => self.detail_error.set(Some(error)),
            }
            self.detail_loading.set(false);
        });
    }

    /// The listed row of an environment, for its health and status.
    fn environment_row(self, environment: &str) -> Option<EnvironmentRow> {
        self.applications.with(|rows| {
            rows.iter()
                .flat_map(|row| &row.environments)
                .find(|row| row.environment.id.as_str() == environment)
                .cloned()
        })
    }

    /// The application and dashboard address of an environment in the current
    /// listing, or `None` when it is not listed (for example after deletion).
    fn environment_link(self, environment: &str) -> Option<(String, String)> {
        self.applications.with(|rows| {
            rows.iter().find_map(|row| {
                row.environments
                    .iter()
                    .find(|listed| listed.environment.id.as_str() == environment)
                    .map(|listed| {
                        (
                            format!("{}/{}", row.application.name, listed.environment.name),
                            environment_href(row.application.id.as_str(), environment),
                        )
                    })
            })
        })
    }

    /// A link to an environment in the current listing, or its ID when it is
    /// not listed.
    fn environment_label(self, environment: &str) -> AnyView {
        self.environment_link(environment).map_or_else(
            || view! { <code>{environment.to_owned()}</code> }.into_any(),
            |(label, href)| view! { <A href={href}>{label}</A> }.into_any(),
        )
    }

    /// Alert shown while the last refresh failed, titled by whether the daemon
    /// was unreachable, and a notice while the shown data is from an earlier
    /// refresh.
    fn refresh_notices(self) -> impl IntoView {
        view! {
            <div
                class="stack-sm"
                style="margin-bottom:16px"
                hidden={move || self.refresh_error.get().is_none()}
            >
                {move || {
                    self.refresh_error
                        .get()
                        .map(|message| {
                            let title = if self.connection.get() == ConnectionState::Unreachable {
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
            <div
                class="stack-sm"
                style="margin-bottom:16px"
                hidden={move || self.data_state.get() != DataState::Stale}
            >
                {notice(Tone::Warn, "Showing the last successful view; the latest refresh failed.")}
            </div>
        }
    }
}

/// The current version of `initial` in `list`, matched by `key`, or
/// `initial` once the list dropped it. `For` keys its rows once, so rows
/// whose records change in place read them through this.
fn live_row<T, K>(list: RwSignal<Vec<T>>, initial: T, key: fn(&T) -> &K) -> Signal<T>
where
    T: Clone + Send + Sync + 'static,
    K: PartialEq + ?Sized + 'static,
{
    Signal::derive(move || {
        list.with(|items| {
            items
                .iter()
                .find(|item| key(item) == key(&initial))
                .cloned()
                .unwrap_or_else(|| initial.clone())
        })
    })
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
                    <Route path={path!("settings")} view={observability::HostPage} />
                    <Route path={path!("accounts")} view={auth::AccountsPage} />
                    <Route path={path!("builds")} view={builds::BuildsPage} />
                    <Route path={path!("events")} view={observability::HistoryPage} />
                    <Route path={path!("errors")} view={observability::ErrorsPage} />
                    <Route path={path!("errors/:id")} view={observability::DiagnosticPage} />
                    <Route path={path!("system")} view={observability::SystemPage} />
                    <Route path={path!("analytics")} view={observability::AnalyticsPage} />
                    <Route path={path!("notifications")} view={notifications::NotificationsPage} />
                    <Route path={path!("audit")} view={observability::AuditPage} />
                    <Route path={path!("applications/:id")} view={ApplicationDetailPage} />
                    <Route
                        path={path!("applications/:id/services/:service")}
                        view={ApplicationDetailPage}
                    />
                    <Route
                        path={path!("applications/:id/environments/:environment")}
                        view={ApplicationDetailPage}
                    />
                    <Route
                        path={path!("applications/:id/previews/:preview")}
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
        Callback::new(move |()| signals.start_refresh(client.clone(), controller, true))
    };
    provide_context(DashboardContext {
        signals,
        client: client.clone(),
        refresh,
    });

    let visibility_listener = window_event_listener(ev::visibilitychange, move |_| {
        controller.update_value(|controller| controller.set_hidden(document_hidden()));
    });
    on_cleanup(move || visibility_listener.remove());

    signals.start_refresh(client.clone(), controller, true);
    signals.spawn_poll_loop(client, controller);

    view! {
        <a class="skip-link" href="#dashboard-main">
            "Skip to main content"
        </a>
        <Sidebar />

        <main id="dashboard-main" class="dashboard-main" tabindex="-1">
            {signals.refresh_notices()}

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

/// Route wrapper for
/// `/applications/:id[/services/:service|/environments/:environment|/previews/:preview]`.
/// Loads the shown environment's or preview's detail into the shared signals when the route
/// changes, and keys `management::ApplicationPage` on the route so it remounts
/// on navigation.
#[component]
fn ApplicationDetailPage() -> impl IntoView {
    let context = dashboard_context();
    let signals = context.signals;
    let params = use_params_map();
    let client = context.client.clone();

    Effect::new(move |_| {
        let (id, environment) = params.with(|params| {
            (
                params.get("id"),
                params.get("environment").or_else(|| params.get("preview")),
            )
        });
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
        signals.load_detail(client.clone(), id);
    });

    view! {
        <For
            each={move || {
                params
                    .with(|params| {
                        params.get("id").map(|id| (id, management::Page::from_params(params)))
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

/// Loads the application, then the environment or preview with the selected
/// ID or, with no selection, its only environment. Applications with several environments
/// (or none) have no environment detail of their own.
async fn environment_detail(
    client: &Client,
    id: &str,
    selected: Option<&str>,
) -> Result<Option<EnvironmentDetailView>, String> {
    let application = client.application(id).await.user_error()?;
    // A selection that no longer exists is reported instead of silently
    // replaced, so later actions never target another environment.
    let environment = match selected {
        Some(selected) => application
            .deployable(selected)
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
        .user_error()
}

impl DashboardSnapshot {
    /// Loads system status, readiness and every application page (bounded by
    /// `MAX_PAGES`), fetching each application's environment statuses and
    /// deployments concurrently.
    /// Only system status and listing failures abort; readiness and per-application
    /// errors are recorded in the snapshot.
    async fn fetch(client: &Client) -> Result<Self, LoadFailure> {
        let system = client.system_status().await?;
        let readiness = client.system_readiness().await.user_error();
        let mut pagination = PaginationState::new();
        let mut cursor = None;
        let mut applications = Vec::new();
        loop {
            let page = client
                .applications_with(&ListApplicationsOptions {
                    cursor: cursor.clone(),
                    limit: Some(PAGE_LIMIT),
                })
                .await?;
            let next_cursor = page.next_cursor.clone();
            // Status reads are independent, so a bounded pool keeps one slow
            // application from serializing the whole refresh.
            let rows = futures_util::stream::iter(
                page.items
                    .into_iter()
                    .map(|application| ApplicationRow::fetch(client, application)),
            )
            .buffered(8)
            .collect::<Vec<_>>()
            .await;
            applications.extend(rows);
            pagination.record_page(next_cursor);
            cursor = pagination.next_cursor().map(str::to_owned);
            if cursor.is_none() || pagination.pages_loaded() >= MAX_PAGES {
                break;
            }
        }
        Ok(Self {
            system,
            readiness,
            applications,
            incomplete: pagination.incomplete(),
        })
    }
}

impl ApplicationRow {
    /// Reads each environment's status and deployments, recording failures
    /// in the row.
    async fn fetch(client: &Client, application: ApplicationSummary) -> Self {
        let mut environments = Vec::with_capacity(application.environments.len());
        let mut deployments = Vec::new();
        let mut deployment_error = None;
        for environment in &application.environments {
            let id = environment.id.as_str();
            let (status, status_error) = match client.environment_status(id).await {
                Ok(status) => (Some(status), None),
                Err(error) => (None, Some(error.user_message())),
            };
            match client.deployments(id, None).await {
                Ok(page) => deployments.extend(page.items),
                Err(error) => deployment_error = Some(error.user_message()),
            }
            environments.push(EnvironmentRow {
                environment: environment.clone(),
                status,
                status_error,
            });
        }
        Self {
            application,
            environments,
            deployments,
            deployment_error,
        }
    }
}

/// How the dashboard reads a failed request.
trait ClientErrorExt {
    /// User-facing message.
    fn user_message(&self) -> String;
    /// Whether the request failed before a response was received, so the
    /// daemon may or may not have applied it.
    fn transport(&self) -> bool;
    /// Whether the daemon refused the request for want of a valid session.
    fn unauthorized(&self) -> bool;
    /// The persisted diagnostic occurrence attached to an API failure.
    fn diagnostic_id(&self) -> Option<String>;
}

impl ClientErrorExt for ClientError {
    fn user_message(&self) -> String {
        match self {
            Self::Endpoint { message } => format!("The dashboard endpoint is invalid: {message}"),
            Self::Transport { message, .. } => format!("Could not reach piqueld: {message}"),
            Self::Api { error, .. } => error.message.clone(),
            Self::Decode { .. } | Self::TextDecode { .. } => {
                "The daemon returned an invalid public API response.".into()
            }
        }
    }

    fn transport(&self) -> bool {
        matches!(self, Self::Transport { .. })
    }

    fn unauthorized(&self) -> bool {
        matches!(self, Self::Api { status, .. } if status.as_u16() == 401)
    }

    fn diagnostic_id(&self) -> Option<String> {
        match self {
            Self::Api { error, .. } => error
                .details
                .get("diagnostic_id")
                .and_then(|id| id.as_str())
                .map(str::to_owned),
            _ => None,
        }
    }
}

/// Request results whose failures are shown as user-facing messages.
trait UserError<T> {
    /// Replaces a failure with its user-facing message.
    fn user_error(self) -> Result<T, String>;
}

impl<T> UserError<T> for Result<T, ClientError> {
    fn user_error(self) -> Result<T, String> {
        self.map_err(|error| error.user_message())
    }
}

/// Returns whether the browser tab is currently hidden.
fn document_hidden() -> bool {
    browser_window()
        .and_then(|window| window.document())
        .is_some_and(|document| document.hidden())
}

/// Asks the user to confirm `message` in a native dialog; `false` when
/// dismissed or unavailable.
fn confirm(message: &str) -> bool {
    browser_window().is_some_and(|window| window.confirm_with_message(message).unwrap_or(false))
}

/// Reloads the current page.
fn reload() {
    if let Some(window) = browser_window() {
        let _ = window.location().reload();
    }
}

/// Dashboard address of one of `application`'s environments.
fn environment_href(application: &str, environment: &str) -> String {
    format!("/dashboard/applications/{application}/environments/{environment}")
}

/// Address of the events of operation `id`.
fn operation_events(id: &str) -> String {
    format!("/dashboard/events?operation={id}")
}

/// Link to `release` on `application`'s Releases tab.
fn release_link(application: &str, release: &str) -> AnyView {
    let href = format!("/dashboard/applications/{application}?release={release}");
    let release = release.to_owned();
    view! {
        <A href={href}>
            <code>{release}</code>
        </A>
    }
    .into_any()
}
