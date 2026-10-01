//! Dashboard navigation, application directory, and recent deployments.
use super::{connection_label, dashboard_context, health_class, management, row_health};
use crate::state::{ApplicationHealth, ConnectionState, DataState};
use leptos::{CollectView, IntoView, SignalGet, View, component, view};
use leptos_router::A;
use piqueld_client::system::{BackupStatus, DependencyStatus};
use std::rc::Rc;

/// Sidebar with the brand link, dashboard navigation and sign-out button.
pub(super) fn dashboard_header() -> View {
    view! {
        <aside class="sidebar">
            <A class="brand" href="/dashboard/">
                <span class="brand-mark">"p"</span>
                "piqueld"
            </A>
            <nav aria-label="Dashboard navigation">
                <A href="/dashboard/applications" class="nav-link" active_class="active">
                    <span aria-hidden="true">"▤"</span>
                    "Applications"
                </A>
                <A href="/dashboard/settings" class="nav-link" active_class="active">
                    <span aria-hidden="true">"⚙"</span>
                    "Host settings"
                </A>
                <A href="/dashboard/builds" class="nav-link" active_class="active">"Builds"</A>
                <A href="/dashboard/events" class="nav-link" active_class="active">"Events"</A>
                <A href="/dashboard/errors" class="nav-link" active_class="active">"Errors"</A>
                <A href="/dashboard/system" class="nav-link" active_class="active">"Daemon status"</A>
                <A href="/dashboard/analytics" class="nav-link" active_class="active">"Analytics"</A>
                <A href="/dashboard/notifications" class="nav-link" active_class="active">"Notifications"</A>
                <A href="/dashboard/accounts" class="nav-link" active_class="active">"Accounts"</A>
                <super::auth::Logout />
            </nav>
        </aside>
    }
    .into_view()
}

/// Overview page: application counts by health, system readiness and the
/// newest deployments, all read from the shared dashboard signals.
#[component]
pub(super) fn OverviewPage() -> impl IntoView {
    let signals = dashboard_context().signals;
    view! {
        <header class="page-heading">
            <h1>"Overview"</h1>
            <management::CreateApplication />
        </header>
        <div class="metrics">
            <A href="/dashboard/applications" class="metric">
                <span>"Applications"</span>
                <strong>{move || signals.applications.get().len()}</strong>
            </A>
            <div class="metric">
                <span>"Running"</span>
                <strong>
                    {move || {
                        signals
                            .applications
                            .get()
                            .iter()
                            .filter(|row| row_health(row) == ApplicationHealth::Converged)
                            .count()
                    }}
                </strong>
            </div>
            <div class="metric">
                <span>"Needs attention"</span>
                <strong>
                    {move || {
                        signals
                            .applications
                            .get()
                            .iter()
                            .filter(|row| {
                                matches!(
                                    row_health(row),
                                    ApplicationHealth::Failed | ApplicationHealth::Degraded
                                )
                            })
                            .count()
                    }}
                </strong>
            </div>
        </div>
        <ReadinessPanel />
        <RecentDeployments />
    }
}

/// Daemon connectivity and dependency readiness cards (database, Docker, Swarm,
/// ingress), with a button that triggers a manual dashboard refresh.
#[component]
pub(super) fn ReadinessPanel() -> impl IntoView {
    let context = dashboard_context();
    let signals = context.signals;
    let refresh = Rc::clone(&context.refresh);

    view! {
        <section class="readiness-panel" aria-labelledby="system-readiness-heading">
            <header class="section-heading readiness-heading">
                <div>
                    <h2 id="system-readiness-heading">"System status"</h2>
                    <p>"Daemon connectivity and the services required to deploy applications."</p>
                </div>
                <button
                    disabled={move || signals.refreshing.get()}
                    on:click={move |_| refresh()}
                >
                    {move || if signals.refreshing.get() { "Refreshing…" } else { "Refresh" }}
                </button>
            </header>
            <div aria-live="polite">
                {move || {
                    signals
                        .readiness_error
                        .get()
                        .map(|error| view! { <p class="readiness-error">"Readiness check failed: " {error}</p> })
                }}
                <div class="readiness-dependencies">
                    {move || connection_readiness(signals.connection.get())}
                    {move || signals.system.get().map(|system| backup_readiness(&system.backup))}
                    {move || {
                        signals.readiness.get().map(|status| {
                            view! {
                                {dependency_readiness("Database", status.database)}
                                {dependency_readiness("Docker Engine", status.docker)}
                                {dependency_readiness("Swarm manager", status.swarm)}
                                {readiness_card("HTTP ingress", if status.ingress.healthy {"ready"} else {"failed"}, if !status.ingress.healthy {"Unhealthy"} else if !status.ingress.enabled {"Disabled"} else {"Ready"}, &status.ingress.message)}
                            }
                        })
                    }}
                </div>
            </div>
        </section>
    }
}

/// Readiness card for the browser's connection to the daemon itself.
fn connection_readiness(state: ConnectionState) -> View {
    let (visual_state, message) = match state {
        ConnectionState::Loading => ("pending", "Waiting for the daemon"),
        ConnectionState::Reachable => ("ready", "The API is responding"),
        ConnectionState::Failed => ("failed", "The daemon returned an error"),
        ConnectionState::Unreachable => ("failed", "The dashboard cannot connect to piqueld"),
    };
    readiness_card(
        "piqueld daemon",
        visual_state,
        connection_label(state),
        message,
    )
}

fn backup_readiness(status: &BackupStatus) -> View {
    let now_ms = js_sys::Date::now()
        .to_string()
        .parse::<i64>()
        .unwrap_or_default();
    let (state, label) = if status.stale {
        ("pending", "Stale")
    } else {
        ("ready", "Recent")
    };
    readiness_card("Backups", state, label, &status.summary(now_ms))
}

/// Readiness card for one daemon dependency.
fn dependency_readiness(name: &'static str, status: DependencyStatus) -> View {
    let (state, label, message) = match status {
        DependencyStatus::Ready => ("ready", "Ready", "Available".to_owned()),
        DependencyStatus::Failed { message } => ("failed", "Failed", message),
    };
    readiness_card(name, state, label, &message)
}

/// One readiness card; `state` (`ready`, `pending`, `failed`) drives its `data-state` styling.
fn readiness_card(
    name: &'static str,
    state: &'static str,
    label: &'static str,
    message: &str,
) -> View {
    view! {
        <article class="readiness-dependency" data-state={state}>
            <header>
                <strong>{name}</strong>
                <span class="readiness-state">
                    <span class="readiness-dot" aria-hidden="true"></span>
                    {label}
                </span>
            </header>
            <p>{message.to_owned()}</p>
        </article>
    }
    .into_view()
}

/// Application directory linking to each application's detail page, with
/// loading/empty states and recent deployments.
#[component]
pub(super) fn ApplicationsPage() -> impl IntoView {
    let signals = dashboard_context().signals;
    view! {
        <header class="page-heading">
            <h1>"Applications"</h1>
            <management::CreateApplication />
        </header>
        <section class="directory" aria-label="Applications">
            {move || {
                signals
                    .applications
                    .get()
                    .into_iter()
                    .map(|row| {
                        let health = row_health(&row);
                        view! {
                            <A
                                class="application-row"
                                href={format!("/dashboard/applications/{}", row.application.id)}
                            >
                                <span class="app-icon" aria-hidden="true">
                                    "▤"
                                </span>
                                <strong>{row.application.name}</strong>
                                <span class={health_class(health)}>{health.label()}</span>
                                <span class="row-arrow" aria-hidden="true">
                                    "→"
                                </span>
                            </A>
                        }
                    })
                    .collect_view()
            }}
            {move || match signals.data_state.get() {
                DataState::Loading => {
                    view! {
                        <p class="empty-state" role="status">
                            "Loading applications…"
                        </p>
                    }
                        .into_view()
                }
                DataState::Empty => {
                    view! { <p class="empty-state">"No applications yet."</p> }.into_view()
                }
                _ => ().into_view(),
            }}
        </section>
        <RecentDeployments />
    }
}

/// The three newest deployments across all loaded applications, plus
/// per-application deployment errors and an incomplete-list warning.
/// Three snapshots per application are sufficient to find the three newest overall.
#[component]
fn RecentDeployments() -> impl IntoView {
    let signals = dashboard_context().signals;
    view! {
        <section class="recent-deployments">
            <header class="section-heading">
                <h2>"Recent deployments"</h2>
            </header>
            {move || {
                signals
                    .pagination_incomplete
                    .get()
                    .then(|| {
                        view! {
                            <p class="conflict-notice">
                                "Application list is incomplete; some deployments may be missing."
                            </p>
                        }
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
                                view! {
                                    <p class="form-error" role="alert">
                                        {format!("{}: {error}", row.application.name)}
                                    </p>
                                }
                            })
                    })
                    .collect_view()
            }}
            <div class="deployment-table">
                <div class="deployment-row table-heading">
                    <span>"Application"</span>
                    <span>"Status"</span>
                    <span>"Revision"</span>
                    <span>"Created"</span>
                </div>
                {move || {
                    let mut deployments = signals
                        .applications
                        .get()
                        .into_iter()
                        .flat_map(|row| {
                            row.deployments
                                .into_iter()
                                .map(move |deployment| (row.application.name.clone(), deployment))
                        })
                        .collect::<Vec<_>>();
                    deployments
                        .sort_by(|a, b| {
                            b.1
                                .operation
                                .created_at_ms
                                .cmp(&a.1.operation.created_at_ms)
                                .then_with(|| b.1.operation.id.cmp(&a.1.operation.id))
                        });
                    if deployments.is_empty() {
                        return view! {
                            <p class="empty-state">
                                {if signals.data_state.get() == DataState::Loading {
                                    "Loading deployments…"
                                } else {
                                    "No recent deployments."
                                }}
                            </p>
                        }
                            .into_view();
                    }
                    deployments
                        .into_iter()
                        .take(3)
                        .map(|(name, deployment)| {
                            view! { <RecentDeployment name={name} deployment={deployment} /> }
                        })
                        .collect_view()
                }}
            </div>
        </section>
    }
}

/// Table row linking to the deployment on its application page.
#[component]
fn RecentDeployment(name: String, deployment: piqueld_client::DeploymentView) -> impl IntoView {
    let op = deployment.operation;
    view! {
        <A
            class="deployment-row"
            href={format!("/dashboard/applications/{}?deployment={}", op.application_id, op.id)}
        >
            <strong>{name}</strong>
            <span>
                <span class="deployment-state" data-state={op.state.as_str()}>
                    {op.state.as_str()}
                </span>
            </span>
            <span>{format!("#{}", op.generation)}</span>
            <span>{management::timestamp(op.created_at_ms)}</span>
        </A>
    }
}
