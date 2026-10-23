//! Service directory and individual service configuration pages.
use super::super::confirm;
use super::super::ui::{Icon, Tabs, Tone, empty, health_badge, icon, notice};
use super::{
    Draft, EditorFeedback, back_link, editor, logs::ApplicationLogs, settings::ServiceGroup,
};
use crate::editor::Section;
use crate::state::ApplicationHealth;
use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::components::A;
use leptos_router::hooks::use_navigate;
use piqueld_client::{
    Source,
    edit::{ApplicationEdit, ServiceEdit},
};

const SERVICE_TABS: [&str; 11] = [
    Section::General.title(),
    Section::Environment.title(),
    Section::Process.title(),
    Section::Storage.title(),
    Section::Files.title(),
    Section::Health.title(),
    Section::Dependencies.title(),
    Section::Rollout.title(),
    Section::Telemetry.title(),
    Section::Resources.title(),
    "Logs",
];

/// Saved services of the application, each linking to its service editor page.
#[component]
pub(super) fn ServiceList() -> impl IntoView {
    let context = editor();
    let signals = context.signals();
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
                            Source::Image { image } => image.to_string(),
                            Source::Git { repository, .. } => format!("Git · {repository}"),
                        };
                        let runtime = observed(&service.name);
                        view! {
                            <A
                                attr:class="list-row"
                                href={format!(
                                    "/dashboard/applications/{id}/services/{}",
                                    service.name,
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

/// Dirty group of the service page's rename form.
const NAME_FORM: &str = "service-name";

/// Service page: the service's renamable title, one tab per `Section` settings
/// group plus a scoped log tab, and a remove button that saves the service's
/// removal and returns to the service list.
#[component]
pub(super) fn ServiceEditor(name: String) -> impl IntoView {
    let context = editor();
    let app_href = move || format!("/dashboard/applications/{}?tab=services", context.id());
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
                )} {back_link(app_href, "Back to services")}
            </div>
        }
        .into_any();
    }
    let navigate = use_navigate();
    let remove_name = name.clone();
    let remove = move |_| {
        if !confirm(
            "Remove this service and its routes from saved configuration? Its running containers remain until Deploy.",
        ) {
            return;
        }
        let navigate = navigate.clone();
        let href = app_href();
        context.save(
            ApplicationEdit::RemoveService(remove_name.clone()),
            move |_| navigate(&href, NavigateOptions::default()),
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
                <ServiceTitle name={name} />
            </div>
            <div class="page-actions">
                <button
                    type="button"
                    class="btn btn-danger"
                    disabled={move || context.action_blocked()}
                    on:click={remove}
                >
                    "Remove service"
                </button>
            </div>
        </header>
        <EditorFeedback />
        <Tabs label="Service sections" options={&SERVICE_TABS} selected={selected} />
        <div class="stack">
            <Show when={move || selected.get() == "Logs"}>
                <ServiceLogs name={log_service.clone()} />
            </Show> {groups}
        </div>
    }
    .into_any()
}

/// Service title with an inline rename form behind the pencil button. Saving
/// the new name also repoints the routes, jobs, exports, and dependencies naming
/// the service, then replaces the page with the renamed service's. Unsaved
/// edits to other forms block renaming, as the page remounts.
#[component]
fn ServiceTitle(name: String) -> impl IntoView {
    let context = editor();
    let navigate = use_navigate();
    let editing = RwSignal::new(false);
    let draft = Draft::new(NAME_FORM.into(), name);
    let save = move || {
        let renamed = draft.value.get_untracked();
        let href = format!(
            "/dashboard/applications/{}/services/{renamed}",
            context.id()
        );
        let navigate = navigate.clone();
        context.save(
            ApplicationEdit::Service {
                name: draft.baseline.get_untracked(),
                edit: ServiceEdit::Name(renamed),
            },
            move |_| {
                let replace = NavigateOptions {
                    replace: true,
                    ..NavigateOptions::default()
                };
                navigate(&href, replace);
            },
        );
    };
    draft.rename_title(
        "Rename service",
        "Service name",
        editing,
        move || context.change_blocked(Some(NAME_FORM)),
        save,
    )
}

/// Logs of one service in the application's only environment. With several
/// environments, links to each environment's logs instead. Remounts when the
/// target environment changes, so polling never keeps a stale target.
#[component]
fn ServiceLogs(name: String) -> impl IntoView {
    let context = editor();
    move || {
        if context.environment.get().is_some() {
            return view! { <ApplicationLogs fixed_service={name.clone()} /> }.into_any();
        }
        view! {
            <div class="stack-sm">
                {notice(
                    Tone::Info,
                    "Logs are read per environment. Choose one to read this service's logs.",
                )}
                <div class="btn-group">
                    {move || {
                        context
                            .saved
                            .get()
                            .environments
                            .into_iter()
                            .map(|environment| {
                                view! {
                                    <A
                                        attr:class="btn"
                                        href={format!(
                                            "{}?tab=logs",
                                            context.environment_href(environment.id.as_str()),
                                        )}
                                    >
                                        {environment.name.to_string()}
                                    </A>
                                }
                            })
                            .collect_view()
                    }}
                </div>
            </div>
        }
        .into_any()
    }
}
