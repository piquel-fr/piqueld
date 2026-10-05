//! Application editor state and page composition. Polling never replaces local edits.
mod deployments;
mod environments;
mod jobs;
mod logs;
mod navigation;
mod routes;
mod secrets;
mod services;
mod settings;

use super::runtime::RuntimeOverview;
use super::ui::{Icon, Modal, PageHeader, Tabs, Tone, health_badge, icon, notice, text_input};
use super::{client_error_message, dashboard_context, row_health};

use deployments::{DeploymentActions, DeploymentHistory};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::NavigateOptions;
use leptos_router::components::A;
use leptos_router::hooks::{query_signal_with_options, use_navigate};
use leptos_router::params::ParamsMap;
use logs::ApplicationLogs;
pub(super) use navigation::HistoryGuard;
use navigation::guard_navigation;
use piqueld_client::{
    ApplicationManifest, ApplicationSpec, ApplicationView, Client, ClientError, Metadata,
    edit::{ApplicationEdit, EditOptions},
};
use secrets::ApplicationSecrets;
use settings::{MetadataSettings, NewService, RepositorySettings, VolumeSettings};
use std::collections::BTreeSet;

const APPLICATION_TABS: [&str; 11] = [
    "Overview",
    "Services",
    "Source",
    "Routes",
    "Volumes",
    "Jobs",
    "Secrets",
    "Deployments",
    "Builds",
    "Logs",
    "Events",
];

/// State shared by every section of one application editor, provided as context.
#[derive(Clone, Copy)]
struct EditorContext {
    dashboard: StoredValue<super::DashboardContext>,
    saved: RwSignal<ApplicationView>,
    /// Environment whose runtime, history, and secrets are shown; `None` until
    /// its detail loads. Changes only when another environment is selected.
    environment: Memo<Option<String>>,
    dirty: RwSignal<BTreeSet<String>>,
    busy: RwSignal<bool>,
    uncertain: RwSignal<bool>,
    error: RwSignal<Option<String>>,
    diagnostic_id: RwSignal<Option<String>>,
    notice: RwSignal<String>,
    tab: RwSignal<&'static str>,
}
impl EditorContext {
    /// The saved manifest, tracked so lists re-render after every save.
    fn manifest(self) -> ApplicationManifest {
        self.saved.with(|saved| saved.application.to_manifest())
    }
    fn id(self) -> String {
        self.saved
            .with_untracked(|saved| saved.application.id().to_string())
    }
    /// The shown environment's ID, read once by components keyed on it.
    fn environment_id(self) -> String {
        self.environment.get_untracked().unwrap_or_default()
    }
    fn selected_environment(self) -> Option<piqueld_client::EnvironmentView> {
        let id = self.environment.get()?;
        self.saved.with(|saved| {
            saved
                .environments
                .iter()
                .find(|env| env.id.as_str() == id)
                .cloned()
        })
    }
    /// Runtime actions require a loaded, live environment, including after deletion.
    fn environment_action_blocked(self) -> bool {
        let signals = self.dashboard.with_value(|d| d.signals);
        self.action_blocked()
            || self
                .selected_environment()
                .is_none_or(|env| env.delete_intent)
            || signals.detail_loading.get()
            || signals.detail_error.get().is_some()
            || signals.detail.with(|detail| {
                detail.as_ref().is_none_or(|detail| {
                    Some(detail.environment.id.to_string()) != self.environment.get()
                })
            })
    }
    /// Keeps service navigation scoped to the selected environment.
    fn environment_query(self) -> String {
        let mut query = ParamsMap::new();
        if let Some(id) = self.environment.get() {
            query.insert("environment", id);
        }
        query.to_query_string()
    }
    fn name(self) -> String {
        self.saved
            .with(|saved| saved.application.metadata().name.to_string())
    }
    /// Whether runtime configuration is owned by a Git manifest instead of these forms.
    fn managed(self) -> bool {
        self.saved
            .with(|saved| saved.application.spec().manifest.is_some())
    }
    /// Whether saves are currently disallowed.
    fn blocked(self) -> bool {
        self.busy.get() || self.uncertain.get()
    }
    /// Whether deploy/delete actions are disallowed: also blocked by unsaved edits
    /// or a pending deletion.
    fn action_blocked(self) -> bool {
        self.blocked() || !self.dirty.get().is_empty() || self.saved.get().delete_intent
    }
    /// Replaces the editor error and clears any diagnostic link.
    fn set_error(self, error: Option<String>) {
        self.diagnostic_id.set(None);
        self.error.set(error);
    }
    /// Shows a request failure. Transport failures mark the editor `uncertain`
    /// because the server may or may not have applied the request.
    fn failure(self, error: &ClientError) {
        self.set_error(Some(client_error_message(error)));
        self.diagnostic_id.set(diagnostic_id(error));
        if matches!(error, ClientError::Transport { .. }) {
            self.uncertain.set(true);
            self.set_error(Some("The request outcome is unknown. Reload saved configuration and deployment history before another action.".into()));
        }
    }
    /// Applies `edit` to the saved manifest, validates it locally, then sends it
    /// guarded by the saved generation. A transport failure is retried once with the
    /// same request ID so the server can deduplicate it. On success the saved view is
    /// updated locally, `on_saved` runs, and the dashboard refreshes.
    fn save(self, edit: ApplicationEdit, on_saved: Callback<ApplicationView>) {
        if self.blocked() {
            return;
        }
        self.set_error(None);
        let mut manifest = self.manifest();
        if let Err(error) = edit.clone().apply(&mut manifest) {
            self.set_error(Some(error.to_string()));
            return;
        }
        let validated = match manifest.validate() {
            Ok(v) => v,
            Err(error) => {
                self.set_error(Some(error.to_string()));
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
                self.set_error(Some(error));
                return;
            }
        };
        self.busy.set(true);
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
                    on_saved.run(updated);
                    self.notice.set("Saved.".into());
                    self.dashboard.with_value(|d| d.refresh.run(()));
                }
                Err(error) => self.failure(&error),
            }
            self.busy.set(false);
        });
    }
}
/// Returns the enclosing `EditorContext`.
fn editor() -> EditorContext {
    use_context().expect("application editor context")
}
/// Returns the persisted diagnostic occurrence attached to an API failure.
fn diagnostic_id(error: &ClientError) -> Option<String> {
    match error {
        ClientError::Api { error, .. } => error
            .details
            .get("diagnostic_id")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        _ => None,
    }
}

/// Whether the request failed before a response was received.
fn transport_failure(error: &ClientError) -> bool {
    matches!(error, ClientError::Transport { .. })
}
/// Client tagged with a fresh random request ID, used for mutations so a retried
/// request is recognised as a replay rather than applied twice.
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
    let id = bytes.iter().fold(String::new(), |mut hex, byte| {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
        hex
    });
    Ok(Client::browser().with_request_id(id))
}
/// Tracks whether `draft` differs from `baseline`, keeping `key` in the editor's
/// dirty set accordingly and removing it when the owning view unmounts.
fn dirty_group<T: Clone + PartialEq + Send + Sync + 'static>(
    key: String,
    draft: RwSignal<T>,
    baseline: RwSignal<T>,
) {
    let context = editor();
    let cleanup = key.clone();
    Effect::new(move |_| {
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

/// Save and discard buttons for one settings group, shown only while it has edits.
fn save_actions<T: Clone + PartialEq + Send + Sync + 'static>(
    draft: RwSignal<T>,
    baseline: RwSignal<T>,
    save: impl Fn() + Copy + 'static,
    disabled: impl Fn() -> bool + Copy + Send + Sync + 'static,
) -> impl IntoView {
    view! {
        <div class="form-actions" hidden={move || draft.get() == baseline.get()}>
            <button
                type="button"
                class="btn btn-primary"
                disabled={move || draft.get() == baseline.get() || disabled()}
                on:click={move |_| save()}
            >
                "Save changes"
            </button>
            <button
                type="button"
                class="btn btn-ghost"
                on:click={move |_| draft.set(baseline.get_untracked())}
            >
                "Discard"
            </button>
            <span class="status">"Unsaved edits"</span>
        </div>
    }
}

/// "Create application" button and modal. Validates the name locally, creates an
/// empty application (retrying once on transport failure), refreshes the
/// dashboard and navigates to the new application.
#[component]
pub(super) fn CreateApplication() -> impl IntoView {
    let opened = RwSignal::new(false);
    let name = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
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
                    refresh.run(());
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
        <button type="button" class="btn btn-primary" on:click={move |_| opened.set(true)}>
            {icon(Icon::Plus)}
            "New application"
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
            <form
                class="stack-sm"
                on:submit={move |event| {
                    event.prevent_default();
                    create(());
                }}
            >
                <fieldset class="stack-sm" disabled={move || busy.get()}>
                    <p class="hint">
                        "The application starts empty. Add services, routes, and volumes afterwards, then deploy."
                    </p>
                    {text_input("Application name", name, String::clone, |v, s| *v = s)}
                </fieldset>
                {move || error.get().map(|e| notice(Tone::Bad, e))}
                <div class="form-actions">
                    <button type="submit" class="btn btn-primary" disabled={move || busy.get()}>
                        "Create application"
                    </button>
                </div>
            </form>
        </Modal>
    }
}

/// Loads saved application `id` once and mounts `ApplicationEditor`, either for
/// the whole application or for one `service`.
#[component]
pub(super) fn ApplicationPage(id: String, service: Option<String>) -> impl IntoView {
    let initial = RwSignal::new(None::<ApplicationView>);
    let error = RwSignal::new(None::<String>);
    spawn_local(async move {
        match Client::browser().application(&id).await {
            Ok(app) => initial.set(Some(app)),
            Err(e) => error.set(Some(client_error_message(&e))),
        }
    });
    view! {
        {move || {
            error
                .get()
                .map(|e| {
                    view! {
                        <div class="stack-sm">
                            {notice(Tone::Bad, e)} <div class="btn-group">
                                <A attr:class="btn" href="/dashboard/applications">
                                    {icon(Icon::ArrowLeft)}
                                    "Back to applications"
                                </A>
                            </div>
                        </div>
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

/// Provides `EditorContext`, guards navigation while edits are unsaved, and renders
/// either a single service editor or the tabbed application editor. The initial
/// tab comes from the `deployment` or `tab=services` query parameters.
#[component]
fn ApplicationEditor(initial: ApplicationView, service: Option<String>) -> impl IntoView {
    let query = leptos_router::hooks::use_query_map();
    let dashboard = dashboard_context();
    let selected = dashboard.signals.selected_environment;
    let context = EditorContext {
        dashboard: StoredValue::new(dashboard),
        saved: RwSignal::new(initial),
        environment: Memo::new(move |_| selected.get()),
        dirty: RwSignal::new(BTreeSet::new()),
        busy: RwSignal::new(false),
        uncertain: RwSignal::new(false),
        error: RwSignal::new(None),
        diagnostic_id: RwSignal::new(None),
        notice: RwSignal::new(String::new()),
        tab: RwSignal::new(if query.with(|q| q.get("deployment").is_some()) {
            "Deployments"
        } else if query.with(|q| q.get("tab").is_some_and(|tab| tab == "services")) {
            "Services"
        } else {
            "Overview"
        }),
    };
    provide_context(context);
    let signals = context.dashboard.with_value(|d| d.signals);
    // Refresh environment metadata without replacing shared configuration drafts.
    Effect::new(move |_| {
        if let Some(application) = signals.applications.with(|rows| {
            rows.iter()
                .find(|row| row.application.id.as_str() == context.id())
                .map(|row| row.application.clone())
        }) {
            if context.saved.with_untracked(|saved| {
                saved.environments != application.environments
                    || saved.delete_intent != application.delete_intent
            }) {
                context.saved.update(|saved| {
                    saved.environments = application.environments;
                    saved.delete_intent = application.delete_intent;
                });
            }
        }
    });
    guard_navigation(context.dirty);
    if let Some(name) = service {
        return view! { <services::ServiceEditor name={name} /> }.into_any();
    }
    let id = context.id();
    let health = move || {
        let id = context.id();
        signals.applications.with(|rows| {
            rows.iter()
                .find(|row| row.application.id.to_string() == id)
                .map(row_health)
        })
    };
    view! {
        <nav class="breadcrumb" aria-label="Breadcrumb">
            <A href="/dashboard/applications">"Applications"</A>
            {icon(Icon::ChevronRight)}
            <span>{move || context.name()}</span>
        </nav>
        <header class="detail-head">
            <div class="detail-title">
                <MetadataSettings />
                {move || health().map(health_badge)}
            </div>
            <div class="page-actions">
                <EnvironmentSelector />
                <environments::EnvironmentManager />
                <a
                    class="btn btn-ghost"
                    href={format!("/api/v1/applications/{id}/manifest")}
                    download
                    title="Download the saved manifest as TOML"
                >
                    {icon(Icon::Download)}
                    "Manifest"
                </a>
                <DeploymentActions />
            </div>
        </header>
        <EditorFeedback />
        <Tabs label="Application sections" options={&APPLICATION_TABS} selected={context.tab} />
        <div hidden={move || context.tab.get() != "Overview"}>
            <div class="stack">
                <RuntimeOverview />
                <DeleteApplication />
            </div>
        </div>
        <ApplicationSettings />
        // Environment-scoped sections remount when another environment is selected.
        {move || {
            context
                .environment
                .get()
                .map(|_| {
                    view! {
                        <div hidden={move || context.tab.get() != "Secrets"}>
                            <ApplicationSecrets />
                        </div>
                        <div hidden={move || context.tab.get() != "Deployments"}>
                            <DeploymentHistory />
                        </div>
                        <Show when={move || context.tab.get() == "Builds"}>
                            <super::builds::BuildHistory environment={context.environment_id()} />
                        </Show>
                        <Show when={move || context.tab.get() == "Logs"}>
                            <ApplicationLogs />
                        </Show>
                        <Show when={move || context.tab.get() == "Events"}>
                            <super::observability::EventHistory environment={context.environment_id()} />
                        </Show>
                    }
                })
        }}
    }
    .into_any()
}

/// Selects the environment whose runtime, history, logs, and secrets are shown.
/// Lists the application's environments, refreshed without replacing local edits.
#[component]
fn EnvironmentSelector() -> impl IntoView {
    let context = editor();
    let environments = move || context.saved.with(|saved| saved.environments.clone());
    let (_, set_environment) = query_signal_with_options::<String>(
        "environment",
        NavigateOptions {
            scroll: false,
            ..Default::default()
        },
    );
    let select = move |event| set_environment.set(Some(event_target_value(&event)));
    view! {
        <label class="field" style="max-width:200px">
            <span>"Environment"</span>
            <select
                prop:value={move || context.environment.get().unwrap_or_default()}
                disabled={move || context.blocked() || context.dirty.with(|dirty| !dirty.is_empty())}
                on:change={select}
            >
                {move || {
                    environments()
                        .into_iter()
                        .map(|environment| {
                            let id = environment.id.to_string();
                            view! {
                                <option value={id.clone()} prop:selected={move || context.environment.get().as_deref() == Some(id.as_str())}>
                                    {environment.name.to_string()}
                                </option>
                            }
                        })
                        .collect_view()
                }}
            </select>
        </label>
    }
}

/// Danger-zone card that deletes the application (guarded by the saved
/// generation) after confirmation, then returns to the application list.
#[component]
fn DeleteApplication() -> impl IntoView {
    let context = editor();
    let navigate = use_navigate();
    let refresh = dashboard_context().refresh;
    let delete = move |_| {
        let environments = context.saved.with_untracked(|saved| {
            saved
                .environments
                .iter()
                .map(|environment| environment.name.to_string())
                .collect::<Vec<_>>()
        });
        let message = format!(
            "Delete this application, all its environments ({}), their services, and all deployment history? Docker volume data will be retained.",
            environments.join(", ")
        );
        if !window().confirm_with_message(&message).unwrap_or(false) {
            return;
        }
        context.set_error(None);
        let client = match mutation_client() {
            Ok(client) => client,
            Err(error) => {
                context.set_error(Some(error));
                return;
            }
        };
        let saved = context.saved.get_untracked();
        let navigate = navigate.clone();
        context.busy.set(true);
        spawn_local(async move {
            let confirmed = environments.iter().map(String::as_str).collect::<Vec<_>>();
            match client
                .delete_application_with_preconditions(
                    saved.application.id().as_str(),
                    Some(saved.generation),
                    false,
                    &confirmed,
                )
                .await
            {
                Ok(_) => {
                    refresh.run(());
                    navigate("/dashboard/applications", NavigateOptions::default());
                }
                Err(error) => context.failure(&error),
            }
            context.busy.set(false);
        });
    };
    view! {
        <section class="card card-danger">
            <header>
                <div>
                    <h3>"Delete application"</h3>
                    <p>
                        "Removes every environment's running services and all configuration and deployment history. Docker volumes and their data are retained."
                    </p>
                </div>
                <button
                    type="button"
                    class="btn btn-danger"
                    disabled={move || context.action_blocked()}
                    on:click={delete}
                >
                    "Delete application"
                </button>
            </header>
        </section>
    }
}

/// `/settings` page: read-only daemon configuration grouped by section.
#[component]
pub(super) fn HostPage() -> impl IntoView {
    let settings = RwSignal::new(None);
    let error = RwSignal::new(None::<String>);
    spawn_local(async move {
        match Client::browser().system_configuration().await {
            Ok(config) => settings.set(Some(config)),
            Err(e) => error.set(Some(client_error_message(&e))),
        }
    });
    view! {
        <PageHeader
            title="Host settings"
            description="Effective daemon configuration. Edit the TOML file and restart the daemon to change these values."
        />
        <div class="stack">
            {move || error.get().map(|e| notice(Tone::Bad, e))}
            {move || {
                settings
                    .get()
                    .map(|config| {
                        config
                            .groups
                            .into_iter()
                            .map(|(group, values)| {
                                view! {
                                    <section class="card">
                                        <header>
                                            <h3>{group}</h3>
                                        </header>
                                        <dl class="kv">
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
        </div>
    }
}

/// Save status, error with reload button and diagnostic link, and a conflict
/// notice when the polled detail shows a newer generation than the editor's.
#[component]
fn EditorFeedback() -> impl IntoView {
    let context = editor();
    let signals = dashboard_context().signals;
    let show_status = move || {
        context.busy.get() || (context.dirty.get().is_empty() && !context.notice.get().is_empty())
    };
    let conflict = move || {
        signals.detail.with(|detail| {
            detail
                .as_ref()
                .is_some_and(|d| d.application.generation > context.saved.get().generation)
        })
    };
    view! {
        <div class="stack-sm" style="margin-bottom:16px">
            <p class="hint" role="status" hidden={move || !show_status()}>
                {move || {
                    if context.busy.get() { "Saving…".into() } else { context.notice.get() }
                }}
            </p>
            {move || {
                context
                    .error
                    .get()
                    .map(|e| {
                        notice(
                            Tone::Bad,
                            view! {
                                <span>{e}</span>
                                <span>
                                    "Your form edits have been kept. Reload to review the latest saved configuration."
                                </span>
                                <div class="btn-group">
                                    <button
                                        type="button"
                                        class="btn btn-sm"
                                        on:click={move |_| {
                                            let _ = window().location().reload();
                                        }}
                                    >
                                        "Reload saved configuration"
                                    </button>
                                    {context
                                        .diagnostic_id
                                        .get()
                                        .map(|id| {
                                            view! {
                                                <A
                                                    attr:class="btn btn-sm btn-ghost"
                                                    href={format!("/dashboard/errors/{id}")}
                                                >
                                                    "Diagnostic details"
                                                </A>
                                            }
                                        })}
                                </div>
                            },
                        )
                    })
            }}
            {move || {
                conflict()
                    .then(|| {
                        notice(
                            Tone::Warn,
                            "Configuration changed elsewhere. Your edits are preserved; reload to review the latest version.",
                        )
                    })
            }}
        </div>
    }
}

/// Source, services, routes, volumes and jobs tabs. Service, route, volume and
/// job editing is disabled while the application is managed from a Git manifest.
#[component]
fn ApplicationSettings() -> impl IntoView {
    let context = editor();
    view! {
        <div hidden={move || {
            !matches!(context.tab.get(), "Source" | "Services" | "Routes" | "Volumes" | "Jobs")
        }}>
            <div class="stack">
                <SharedConfigurationNotice />
                {move || {
                    context
                        .managed()
                        .then(|| {
                            notice(
                                Tone::Info,
                                "Runtime configuration is managed in Git. Disconnect the repository in Source to edit services, routes, volumes, and jobs here.",
                            )
                        })
                }} <div hidden={move || context.tab.get() != "Source"}>
                    <RepositorySettings />
                </div> <fieldset disabled={move || context.managed()}>
                    <div hidden={move || context.tab.get() != "Services"}>
                        <div class="section-header">
                            <div>
                                <h2>"Services"</h2>
                                <p>
                                    "Each service runs one image as a replicated Swarm service on the application network."
                                </p>
                            </div>
                            <NewService />
                        </div>
                        <services::ServiceList />
                    </div>
                    <div hidden={move || context.tab.get() != "Routes"}>
                        <routes::RouteSettings />
                    </div>
                    <div hidden={move || context.tab.get() != "Volumes"}>
                        <VolumeSettings />
                    </div>
                    <div hidden={move || context.tab.get() != "Jobs"}>
                        <jobs::JobSettings />
                    </div>
                </fieldset>
            </div>
        </div>
    }
}

/// Appears beside shared application forms, including individual service settings.
#[component]
fn SharedConfigurationNotice() -> impl IntoView {
    notice(
        Tone::Info,
        "Shared across all environments. Saving changes the configuration each environment will use on its next deployment; running deployments keep their current settings.",
    )
}
