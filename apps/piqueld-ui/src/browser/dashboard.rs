//! Dashboard navigation, application directory, and recent deployments.
use super::{
    DashboardContext, connection_label, dashboard_context, health_class, management, row_health,
    status_dot_class,
};
use crate::state::{ApplicationHealth, DataState};
use leptos::{CollectView, IntoView, SignalGet, View, component, view};
use leptos_router::A;
use std::rc::Rc;

pub(super) fn dashboard_header(context: &DashboardContext) -> View {
    let signals = context.signals;
    let refresh = Rc::clone(&context.refresh);
    let location = leptos_router::use_location();
    view! {
        <aside class="sidebar">
            <A class="brand" href="/dashboard/">
                <span class="brand-mark">"p"</span>
                "piqueld"
            </A>
            <nav aria-label="Dashboard navigation">
                <A
                    href="/dashboard/applications"
                    class={move || {
                        if location.pathname.get().contains("/applications") {
                            "nav-link active"
                        } else {
                            "nav-link"
                        }
                    }}
                >
                    <span aria-hidden="true">"▤"</span>
                    "Applications"
                </A>
                <A
                    href="/dashboard/settings"
                    class={move || {
                        if location.pathname.get().ends_with("/settings") {
                            "nav-link active"
                        } else {
                            "nav-link"
                        }
                    }}
                >
                    <span aria-hidden="true">"⚙"</span>
                    "Host settings"
                </A>
            </nav>
            <div class="sidebar-status">
                <p role="status">
                    <span class={move || status_dot_class(signals.connection.get())}></span>
                    {move || connection_label(signals.connection.get())}
                </p>
                <button disabled={move || signals.refreshing.get()} on:click={move |_| refresh()}>
                    {move || if signals.refreshing.get() { "Refreshing…" } else { "Refresh" }}
                </button>
            </div>
        </aside>
    }
    .into_view()
}

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
        <RecentDeployments />
    }
}

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
