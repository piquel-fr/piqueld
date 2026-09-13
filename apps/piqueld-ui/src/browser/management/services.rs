//! Service directory and individual service configuration pages.
use super::{EditorFeedback, editor, settings::ServiceGroup};
use crate::editor::Section;
use leptos::{
    Callback, CollectView, IntoView, SignalGet, SignalSet, SignalWith, SignalWithUntracked,
    component, create_rw_signal, view, window,
};
use leptos_router::{A, NavigateOptions, use_navigate};
use piqueld_client::Source;

#[component]
pub(super) fn ServiceList() -> impl IntoView {
    let context = editor();
    view! {
        <div class="directory" aria-label="Services">
            {move || {
                context
                    .saved
                    .with(|app| {
                        if app.application.to_manifest().spec.services.is_empty() {
                            return view! { <p class="empty-state">"No services yet."</p> }
                                .into_view();
                        }
                        app.application.to_manifest()
                            .spec
                            .services
                            .clone()
                            .into_iter()
                            .map(|service| {
                                let source = match &service.source {
                                    Source::Image { image } => image.clone(),
                                    Source::Git { repository, .. } => repository.url.clone(),
                                };
                                view! {
                                    <A
                                        class="application-row service-row"
                                        href={format!(
                                            "/dashboard/applications/{}/services/{}",
                                            app.application.id(),
                                            service.name,
                                        )}
                                    >
                                        <strong>{service.name.clone()}</strong>
                                        <span class="service-source">{source}</span>
                                        <span class="tag">
                                            {format!("{} replicas", service.replicas)}
                                        </span>
                                        <span class="row-arrow" aria-hidden="true">
                                            "→"
                                        </span>
                                    </A>
                                }
                            })
                            .collect_view()
                    })
            }}
        </div>
    }
}

#[component]
pub(super) fn ServiceEditor(name: String) -> impl IntoView {
    let context = editor();
    if !context.saved.with_untracked(|app| {
        app.application
            .to_manifest()
            .spec
            .services
            .iter()
            .any(|service| service.name == name)
    }) {
        return view! { <p class="empty-state">"Service not found."</p> }.into_view();
    }
    let app_href = context.saved.with_untracked(|app| {
        format!(
            "/dashboard/applications/{}?tab=services",
            app.application.id()
        )
    });
    let navigate = use_navigate();
    let remove_name = name.clone();
    let return_href = app_href.clone();
    let remove = move |_| {
        if !window().confirm_with_message("Remove this service from saved configuration? Its running containers remain until Deploy.").unwrap_or(false) {return;}
        let mut manifest = context.manifest();
        manifest
            .spec
            .services
            .retain(|service| service.name != remove_name);
        let navigate = navigate.clone();
        let href = return_href.clone();
        context.save(
            manifest,
            Callback::new(move |_| navigate(&href, NavigateOptions::default())),
        );
    };
    let managed = move || {
        context
            .saved
            .with(|app| app.application.to_manifest().spec.manifest.is_some())
    };
    let selected = create_rw_signal(Section::General);
    let groups = Section::ALL
        .into_iter()
        .map(|section| {
            view! {
                <div hidden={move || selected.get() != section}>
                    <ServiceGroup name={name.clone()} section={section} />
                </div>
            }
        })
        .collect_view();
    view! {
        <header class="application-heading">
            <h1 class="service-heading">
                <A href={app_href}>
                    {move || context.saved.with(|app| app.application.to_manifest().metadata.name.clone())}
                </A>
                <span class="path-separator">"/"</span>
                {name}
            </h1>
            <button
                class="danger"
                disabled={move || { context.action_blocked() || managed() }}
                on:click={remove}
            >
                "Remove service"
            </button>
        </header>
        <EditorFeedback />
        <nav class="tabs" aria-label="Service sections">
            {Section::ALL
                .into_iter()
                .map(|section| {
                    view! {
                        <button
                            class:active={move || selected.get() == section}
                            aria-current={move || {
                                if selected.get() == section { "page" } else { "false" }
                            }}
                            on:click={move |_| selected.set(section)}
                        >
                            {section.title()}
                        </button>
                    }
                })
                .collect_view()}
        </nav>
        {move || {
            managed()
                .then(|| {
                    view! {
                        <p class="help">
                            "Managed in Git. Edit this service in the repository manifest."
                        </p>
                    }
                })
        }}
        {groups}
    }
    .into_view()
}
