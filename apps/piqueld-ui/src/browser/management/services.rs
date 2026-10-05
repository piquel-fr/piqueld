//! Service directory and individual service configuration pages.
use super::super::ui::{Icon, Tabs, Tone, empty, health_badge, icon, notice};
use super::{EditorFeedback, editor, settings::ServiceGroup};
use crate::editor::Section;
use crate::state::ApplicationHealth;
use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::components::A;
use leptos_router::hooks::use_navigate;
use piqueld_client::{Source, edit::ApplicationEdit};

const SERVICE_TABS: [&str; 9] = [
    Section::General.title(),
    Section::Environment.title(),
    Section::Process.title(),
    Section::Storage.title(),
    Section::Health.title(),
    Section::Dependencies.title(),
    Section::Rollout.title(),
    Section::Resources.title(),
    "Logs",
];

/// Saved services of the application, each linking to its service editor page.
#[component]
pub(super) fn ServiceList() -> impl IntoView {
    let context = editor();
    let signals = context.dashboard.with_value(|d| d.signals);
    // Observed runtime health for one saved service, when the detail has loaded.
    let observed = move |name: &str| {
        signals.detail.with(|detail| {
            detail.as_ref().and_then(|detail| {
                detail
                    .observed
                    .services
                    .iter()
                    .find(|service| service.name == name)
                    .map(|service| {
                        (
                            ApplicationHealth::from_convergence(&service.convergence),
                            service.healthy_replicas,
                            service.desired_replicas,
                        )
                    })
            })
        })
    };
    view! {
        <div class="list" aria-label="Services">
            {move || {
                let manifest = context.manifest();
                if manifest.spec.services.is_empty() {
                    return empty(
                        "No services yet. Add one to describe what this application runs.",
                    );
                }
                let id = context.id();
                manifest
                    .spec
                    .services
                    .into_iter()
                    .map(|service| {
                        let source = match &service.source {
                            Source::Image { image } => image.clone(),
                            Source::Git { repository, .. } => format!("Git · {repository}"),
                        };
                        let runtime = observed(&service.name);
                        view! {
                            <A
                                attr:class="list-row"
                                href={format!(
                                    "/dashboard/applications/{id}/services/{}{}",
                                    service.name,
                                    context.environment_query(),
                                )}
                            >
                                <span class="app-icon" aria-hidden="true">
                                    {icon(Icon::Package)}
                                </span>
                                <span class="title">
                                    {service.name.clone()}
                                    <small title={source.clone()}>{source.clone()}</small>
                                </span>
                                <span class="meta">
                                    {runtime
                                        .map_or_else(
                                            || format!("{} replicas", service.replicas),
                                            |(_, healthy, desired)| {
                                                format!("{healthy} / {desired} healthy")
                                            },
                                        )}
                                </span>
                                {runtime.map(|(health, ..)| health_badge(health))}
                                <span class="chevron" aria-hidden="true">
                                    {icon(Icon::ChevronRight)}
                                </span>
                            </A>
                        }
                    })
                    .collect_view()
                    .into_any()
            }}
        </div>
    }
}

/// Service page: one tab per `Section` settings group plus a scoped log tab, and a
/// remove button that saves the service's removal and returns to the service list.
/// Editing is disabled when the application is managed from a Git manifest.
#[component]
pub(super) fn ServiceEditor(name: String) -> impl IntoView {
    let context = editor();
    let app_href = move || {
        let query = context.environment_query();
        let separator = if query.is_empty() { '?' } else { '&' };
        format!(
            "/dashboard/applications/{}{query}{separator}tab=services",
            context.id()
        )
    };
    if !context
        .manifest()
        .spec
        .services
        .iter()
        .any(|service| service.name == name)
    {
        return view! {
            <div class="stack-sm">
                {notice(
                    Tone::Bad,
                    format!("Service {name} is not part of the saved configuration."),
                )} <div class="btn-group">
                    <A attr:class="btn" href={app_href}>
                        {icon(Icon::ArrowLeft)}
                        "Back to services"
                    </A>
                </div>
            </div>
        }
        .into_any();
    }
    let navigate = use_navigate();
    let remove_name = name.clone();
    let return_href = app_href;
    let remove = move |_| {
        if !window().confirm_with_message("Remove this service and its routes from saved configuration? Its running containers remain until Deploy.").unwrap_or(false) {return;}
        let navigate = navigate.clone();
        let href = return_href();
        context.save(
            ApplicationEdit::RemoveService(remove_name.clone()),
            Callback::new(move |_| navigate(&href, NavigateOptions::default())),
        );
    };
    let selected = RwSignal::new(Section::General.title());
    let log_service = name.clone();
    let groups = Section::ALL
        .into_iter()
        .map(|section| {
            view! {
                <div hidden={move || selected.get() != section.title()}>
                    <ServiceGroup name={name.clone()} section={section} />
                </div>
            }
        })
        .collect_view();
    view! {
        <nav class="breadcrumb" aria-label="Breadcrumb">
            <A href="/dashboard/applications">"Applications"</A>
            {icon(Icon::ChevronRight)}
            <A href={app_href}>{move || context.name()}</A>
            {icon(Icon::ChevronRight)}
            <span>{log_service.clone()}</span>
        </nav>
        <header class="detail-head">
            <div class="detail-title">
                <h1>{name}</h1>
            </div>
            <div class="page-actions">
                <button
                    type="button"
                    class="btn btn-danger"
                    disabled={move || context.action_blocked() || context.managed()}
                    on:click={remove}
                >
                    "Remove service"
                </button>
            </div>
        </header>
        <EditorFeedback />
        <Tabs label="Service sections" options={&SERVICE_TABS} selected={selected} />
        <div class="stack">
            <Show when={move || selected.get() != "Logs"}>
                <super::SharedConfigurationNotice />
            </Show>
            {move || {
                (context.managed() && selected.get() != "Logs")
                    .then(|| {
                        notice(
                            Tone::Info,
                            "This service is managed in Git. Edit it in the repository manifest.",
                        )
                    })
            }} <Show when={move || selected.get() == "Logs"}>
                <super::logs::ApplicationLogs fixed_service={log_service.clone()} />
            </Show> {groups}
        </div>
    }
    .into_any()
}
