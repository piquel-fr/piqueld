//! Application editor state and page composition. Polling never replaces local edits.
mod controls;
mod logs;
use logs::ApplicationLogs;
mod secrets;
use secrets::ApplicationSecrets;
mod deployments;
mod navigation;
mod services;
mod settings;

use super::runtime::{RuntimeDetails, RuntimeSection};
use super::{client_error_message, dashboard_context};

use controls::{Modal, Tabs, text_input};
pub(super) use deployments::timestamp;
use deployments::{DeploymentActions, DeploymentHistory};
use leptos::{
    Callable, Callback, CollectView, IntoView, RwSignal, SignalGet, SignalGetUntracked, SignalSet,
    SignalUpdate, SignalWith, SignalWithUntracked, StoredValue, component, create_effect,
    create_rw_signal, on_cleanup, provide_context, spawn_local, store_value, use_context, view,
    window,
};
use leptos_router::{A, NavigateOptions, use_navigate};
pub(super) use navigation::HistoryGuard;
use navigation::guard_navigation;
use piqueld_client::{
    ApplicationManifest, ApplicationSpec, ApplicationView, Client, ClientError, Metadata,
    edit::{ApplicationEdit, EditOptions},
};
use settings::{MetadataSettings, NewService, RepositorySettings, VolumeSettings};
use std::collections::BTreeSet;

#[derive(Clone, Copy)]
struct EditorContext {
    dashboard: StoredValue<super::DashboardContext>,
    saved: RwSignal<ApplicationView>,
    dirty: RwSignal<BTreeSet<String>>,
    busy: RwSignal<bool>,
    uncertain: RwSignal<bool>,
    error: RwSignal<Option<String>>,
    notice: RwSignal<String>,
    tab: RwSignal<&'static str>,
}
impl EditorContext {
    fn manifest(self) -> ApplicationManifest {
        self.saved
            .with_untracked(|saved| saved.application.to_manifest())
    }
    fn blocked(self) -> bool {
        self.busy.get() || self.uncertain.get()
    }
    fn action_blocked(self) -> bool {
        self.blocked() || !self.dirty.get().is_empty() || self.saved.get().delete_intent
    }
    fn failure(self, error: &ClientError) {
        self.error.set(Some(client_error_message(error)));
        if matches!(error, ClientError::Transport { .. }) {
            self.uncertain.set(true);
            self.error.set(Some("The request outcome is unknown. Reload saved configuration and deployment history before another action.".into()));
        }
    }
    fn save(self, edit: ApplicationEdit, on_saved: Callback<ApplicationView>) {
        if self.blocked() {
            return;
        }
        let mut manifest = self.manifest();
        if let Err(error) = edit.clone().apply(&mut manifest) {
            self.error.set(Some(error.to_string()));
            return;
        }
        let validated = match manifest.validate() {
            Ok(v) => v,
            Err(error) => {
                self.error.set(Some(error.to_string()));
                return;
            }
        };
        let saved = self.saved.get_untracked();
        let options = EditOptions {
            expected_generation: Some(saved.generation),
            ..EditOptions::default()
        };
        let client = match mutation_client() {
            Ok(client) => client,
            Err(error) => {
                self.error.set(Some(error));
                return;
            }
        };
        self.busy.set(true);
        self.error.set(None);
        spawn_local(async move {
            let mut result = client
                .edit_application(saved.application.id().as_str(), &edit, &options)
                .await;
            if result.as_ref().is_err_and(transport_failure) {
                result = client
                    .edit_application(saved.application.id().as_str(), &edit, &options)
                    .await;
            }
            match result {
                Ok(receipt) => {
                    let application = validated.normalize(saved.application.id().clone());
                    let updated = ApplicationView {
                        spec_hash: application.spec_hash(),
                        application,
                        generation: receipt.generation,
                        ..saved
                    };
                    self.saved.set(updated.clone());
                    on_saved.call(updated);
                    self.notice.set("Saved.".into());
                    self.dashboard.with_value(|d| (d.refresh)());
                }
                Err(error) => self.failure(&error),
            }
            self.busy.set(false);
        });
    }
}
fn editor() -> EditorContext {
    use_context().expect("application editor context")
}
fn transport_failure(error: &ClientError) -> bool {
    matches!(error, ClientError::Transport { .. })
}
fn mutation_client() -> Result<Client, String> {
    // Unlike randomUUID, getRandomValues is available on the supported plain
    // HTTP origins. Keep 128 bits of cryptographic entropy for replay identities.
    let error = "Browser could not create a request identity.";
    let mut bytes = [0; 16];
    window()
        .crypto()
        .map_err(|_| error)?
        .get_random_values_with_u8_array(&mut bytes)
        .map_err(|_| error)?;
    let id = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(Client::browser().with_request_id(id))
}
fn dirty_group<T: Clone + PartialEq + 'static>(
    key: String,
    draft: RwSignal<T>,
    baseline: RwSignal<T>,
) {
    let context = editor();
    let cleanup = key.clone();
    create_effect(move |_| {
        let dirty = draft.get() != baseline.get();
        context.dirty.update(|groups| {
            if dirty {
                groups.insert(key.clone());
            } else {
                groups.remove(&key);
            }
        });
    });
    on_cleanup(move || {
        context.dirty.update(|groups| {
            groups.remove(&cleanup);
        });
    });
}
#[component]
pub(super) fn CreateApplication() -> impl IntoView {
    let opened = create_rw_signal(false);
    let name = create_rw_signal(String::new());
    let busy = create_rw_signal(false);
    let error = create_rw_signal(None::<String>);
    let navigate = use_navigate();
    let refresh = dashboard_context().refresh;
    let create = move |()| {
        if busy.get_untracked() {
            return;
        }
        let manifest = ApplicationManifest {
            api_version: "piqueld.dev/v1alpha1".into(),
            kind: "Application".into(),
            metadata: Metadata {
                name: name.get_untracked(),
            },
            spec: ApplicationSpec::default(),
        };
        if let Err(problem) = manifest.clone().validate() {
            error.set(Some(problem.to_string()));
            return;
        }
        let client = match mutation_client() {
            Ok(client) => client,
            Err(problem) => {
                error.set(Some(problem));
                return;
            }
        };
        let navigate = navigate.clone();
        let refresh = refresh.clone();
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let name = manifest.metadata.name;
            let mut result = client.create_application(&name, false).await;
            if result.as_ref().is_err_and(transport_failure) {
                result = client.create_application(&name, false).await;
            }
            match result {
                Ok(saved) => {
                    refresh();
                    navigate(
                        &format!("/dashboard/applications/{}", saved.application_id),
                        NavigateOptions::default(),
                    );
                }
                Err(problem) => error.set(Some(client_error_message(&problem))),
            }
            busy.set(false);
        });
    };
    view! {
        <div class="create-app">
            <button class="primary" on:click={move |_| opened.set(true)}>
                "+ Create application"
            </button>
            <Modal
                title="Create application"
                opened={opened}
                busy={busy}
                on_close={Callback::new(move |()| {
                    name.set(String::new());
                    error.set(None);
                })}
            >
                <form on:submit={move |event| {
                    event.prevent_default();
                    create(());
                }}>
                    <fieldset disabled={move || {
                        busy.get()
                    }}>
                        {text_input("Application name", name, String::clone, |v, s| *v = s)}
                        <div class="form-actions">
                            <button type="submit" class="primary">
                                "Create application"
                            </button>
                        </div>
                    </fieldset>
                    {move || {
                        error
                            .get()
                            .map(|e| {
                                view! {
                                    <p class="form-error" role="alert">
                                        {e}
                                    </p>
                                }
                            })
                    }}
                </form>
            </Modal>
        </div>
    }
}

#[component]
pub(super) fn ApplicationPage(id: String, service: Option<String>) -> impl IntoView {
    let is_service = service.is_some();
    let back = if is_service {
        format!("/dashboard/applications/{id}?tab=services")
    } else {
        "/dashboard/applications".into()
    };
    let initial = create_rw_signal(None::<ApplicationView>);
    let error = create_rw_signal(None::<String>);
    spawn_local(async move {
        match Client::browser().application(&id).await {
            Ok(app) => initial.set(Some(app)),
            Err(e) => error.set(Some(client_error_message(&e))),
        }
    });
    view! {
        <A href={back} class="back-link">
            {if is_service { "← Services" } else { "← Applications" }}
        </A>
        {move || {
            error
                .get()
                .map(|e| {
                    view! {
                        <p class="form-error" role="alert">
                            {e}
                        </p>
                    }
                })
        }}
        {move || {
            initial
                .get()
                .map(|initial| {
                    view! { <ApplicationEditor initial={initial} service={service.clone()} /> }
                })
        }}
    }
}

#[component]
fn ApplicationEditor(initial: ApplicationView, service: Option<String>) -> impl IntoView {
    let context = EditorContext {
        dashboard: store_value(dashboard_context()),
        saved: create_rw_signal(initial),
        dirty: create_rw_signal(BTreeSet::new()),
        busy: create_rw_signal(false),
        uncertain: create_rw_signal(false),
        error: create_rw_signal(None),
        notice: create_rw_signal(String::new()),
        tab: create_rw_signal(
            if leptos_router::use_query_map().with(|q| q.get("deployment").is_some()) {
                "Deployments"
            } else if leptos_router::use_query_map()
                .with(|q| q.get("tab").is_some_and(|tab| tab == "services"))
            {
                "Services"
            } else {
                "Overview"
            },
        ),
    };
    provide_context(context);
    guard_navigation(context.dirty);
    if let Some(name) = service {
        return view! { <services::ServiceEditor name={name} /> }.into_view();
    }
    view! {
        <header class="application-heading">
            <MetadataSettings />
            <div class="header-controls">
                <a
                    class="back-link"
                    href={move || {
                        format!(
                            "/api/v1/applications/{}/manifest",
                            context.saved.get().application.id()
                        )
                    }}
                    download
                >
                    "Download saved manifest"
                </a>
                <DeploymentActions />
            </div>
        </header>
        <EditorFeedback />
        <Tabs
            label="Application sections"
            options={&["Overview", "Source", "Services", "Volumes", "Deployments", "Diagnostics", "Builds", "Logs", "Secrets"]}
            selected={context.tab}
            class="tabs"
        />
        <ApplicationSettings />
        <leptos::Show when=move ||context.tab.get()=="Logs"><ApplicationLogs/></leptos::Show>
        <leptos::Show when=move ||context.tab.get()=="Builds"><super::builds::BuildHistory application=context.saved.with_untracked(|a|a.application.id().to_string())/></leptos::Show>
        <div hidden=move ||context.tab.get()!="Secrets"><ApplicationSecrets/></div>
        <div hidden={move || context.tab.get() != "Deployments"}>
            <DeploymentHistory />
        </div>
        <div hidden={move || context.tab.get() != "Overview"}>
            <RuntimeDetails section={RuntimeSection::Overview} />
            <DeleteApplication />
        </div>
        <div hidden={move || context.tab.get() != "Diagnostics"}>
            <RuntimeDetails section={RuntimeSection::Diagnostics} />
        </div>
    }
    .into_view()
}

#[component]
fn DeleteApplication() -> impl IntoView {
    let context = editor();
    let navigate = use_navigate();
    let refresh = dashboard_context().refresh;
    let delete = move |_| {
        if !window().confirm_with_message("Delete this application, its services, and all deployment history? Docker volume data will be retained.").unwrap_or(false){return;}
        let Ok(client) = mutation_client() else {
            return;
        };
        let saved = context.saved.get_untracked();
        let navigate = navigate.clone();
        let refresh = refresh.clone();
        context.busy.set(true);
        spawn_local(async move {
            match client
                .delete_application_with_generation(
                    saved.application.id().as_str(),
                    Some(saved.generation),
                )
                .await
            {
                Ok(_) => {
                    refresh();
                    navigate("/dashboard/applications", NavigateOptions::default());
                }
                Err(error) => context.failure(&error),
            }
            context.busy.set(false);
        });
    };
    view! {
        <section class="settings-card danger-zone">
            <h3>"Delete application"</h3>
            <p class="help">
                "Removes services and all application history. Docker volumes and their data are retained."
            </p>
            <button class="danger" disabled={move || context.action_blocked()} on:click={delete}>
                "Delete application"
            </button>
        </section>
    }
}

#[component]
pub(super) fn HostPage() -> impl IntoView {
    let settings = create_rw_signal(None);
    let error = create_rw_signal(None::<String>);
    spawn_local(async move {
        match Client::browser().system_configuration().await {
            Ok(config) => settings.set(Some(config)),
            Err(e) => error.set(Some(client_error_message(&e))),
        }
    });
    view! {
        <header class="application-heading">
            <div>
                <h2>"Host settings"</h2>
            </div>
        </header>
        {move || error.get().map(|e| view! { <p class="form-error">{e}</p> })}
        {move || {
            settings
                .get()
                .map(|config| {
                    config
                        .groups
                        .into_iter()
                        .map(|(group, values)| {
                            view! {
                                <section class="settings-card">
                                    <h3>{group}</h3>
                                    <dl class="host-settings">
                                        {values
                                            .into_iter()
                                            .map(|(key, value)| {
                                                view! {
                                                    <dt>{key}</dt>
                                                    <dd>{value}</dd>
                                                }
                                            })
                                            .collect_view()}
                                    </dl>
                                </section>
                            }
                        })
                        .collect_view()
                })
        }}
    }
}

#[component]
fn EditorFeedback() -> impl IntoView {
    let context = editor();
    let signals = dashboard_context().signals;
    view! {
        <p
            class="save-status"
            role="status"
            hidden={move || {
                !context.busy.get()
                    && (!context.dirty.get().is_empty() || context.notice.get().is_empty())
            }}
        >
            {move || {
                if context.busy.get() {
                    "Saving…".into()
                } else {
                    context.notice.get()
                }
            }}
        </p>
        {move || {
            context
                .error
                .get()
                .map(|e| {
                    view! {
                        <div class="form-error" role="alert">
                            <p>{e}</p>
                            <p>
                                "Your form edits have been kept. Reload to review the latest saved configuration."
                            </p>
                            <button on:click={move |_| {
                                let _ = window().location().reload();
                            }}>"Reload saved configuration"</button>
                        </div>
                    }
                })
        }}
        {move || {
            signals
                .detail
                .get()
                .filter(|d| d.application.generation > context.saved.get().generation)
                .map(|_| {
                    view! {
                        <p class="conflict-notice">
                            "Configuration changed elsewhere. Your edits are preserved; reload to review the latest version."
                        </p>
                    }
                })
        }}
    }
}

#[component]
fn ApplicationSettings() -> impl IntoView {
    let context = editor();
    view! {
        <div hidden={move || !matches!(context.tab.get(), "Source" | "Services" | "Volumes")}>
            <div hidden={move || context.tab.get() != "Source"}>
                <RepositorySettings />
            </div>
            {move || {
                context
                    .saved
                    .get()
                    .application.to_manifest()
                    .spec
                    .manifest
                    .is_some()
                    .then(|| {
                        view! {
                            <p class="help">
                                "Managed in Git. Disconnect the repository in Source to edit services and volumes here."
                            </p>
                        }
                    })
            }}
            <fieldset disabled={move || context.saved.get().application.to_manifest().spec.manifest.is_some()}>
                <div hidden={move || context.tab.get() != "Services"}>
                    <div class="list-actions">
                        <NewService />
                    </div>
                    <services::ServiceList />
                </div>
                <div hidden={move || context.tab.get() != "Volumes"}>
                    <VolumeSettings />
                </div>
            </fieldset>
            <div class="mt-4" hidden={move || context.tab.get() != "Services"}>
                <RuntimeDetails section={RuntimeSection::Services} />
            </div>
        </div>
    }
}
