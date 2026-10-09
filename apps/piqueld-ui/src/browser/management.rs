//! Application editor state and page composition. Polling never replaces local edits.
mod deployments;
mod environments;
mod jobs;
mod logs;
mod navigation;
mod previews;
mod releases;
mod routes;
mod secrets;
mod services;
mod settings;
mod variables;

use super::format::timestamp;
use super::runtime::RuntimeOverview;
use super::ui::{
    Icon, Modal, PageHeader, Tabs, Tone, badge, health_badge, icon, notice, text_input,
};
use super::{client_error_message, dashboard_context, environment_row, row_health};
use crate::state::ApplicationHealth;

use deployments::{DeploymentActions, DeploymentHistory};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::NavigateOptions;
use leptos_router::components::A;
use leptos_router::hooks::use_navigate;
use logs::ApplicationLogs;
pub(super) use navigation::HistoryGuard;
use navigation::guard_navigation;
use piqueld_client::{
    ApplicationManifest, ApplicationSpec, ApplicationTemplate, ApplicationView, Client,
    ClientError, Metadata,
    edit::{ApplicationEdit, EditOptions},
};
use secrets::{EnvironmentSecrets, SecretFileSettings};
use settings::{MetadataSettings, NewService, RepositorySettings, VolumeSettings};
use std::collections::BTreeSet;

const APPLICATION_TABS: [&str; 13] = [
    "Overview",
    "Environments",
    "Previews",
    "Services",
    "Source",
    "Variables",
    "Routes",
    "Volumes",
    "Jobs",
    "Secrets",
    "Releases",
    "Builds",
    "Events",
];

const ENVIRONMENT_TABS: [&str; 5] = ["Overview", "Deployments", "Secrets", "Logs", "Events"];

/// Which page of an application the route shows.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Page {
    /// Shared configuration, environments, and application-wide history.
    Application,
    /// One service's shared configuration.
    Service(String),
    /// One environment's runtime, deployments, secrets, logs, and history.
    Environment(String),
    /// One preview's branch, runtime, deployments, secrets, logs, and history.
    Preview(String),
}

impl Page {
    /// Tabs of the page; service pages have their own section tabs.
    const fn tabs(&self) -> &'static [&'static str] {
        match self {
            Self::Environment(_) | Self::Preview(_) => &ENVIRONMENT_TABS,
            Self::Application | Self::Service(_) => &APPLICATION_TABS,
        }
    }

    /// The initial tab: `?deployment=` opens deployments, `?release=`
    /// releases, and `?tab=` names a tab.
    fn initial_tab(&self, query: &leptos_router::params::ParamsMap) -> &'static str {
        let requested = if query.get("deployment").is_some() {
            Some("deployments".to_owned())
        } else if query.get("release").is_some() {
            Some("releases".to_owned())
        } else {
            query.get("tab")
        };
        self.tabs()
            .iter()
            .find(|tab| requested.as_deref() == Some(tab.to_lowercase().as_str()))
            .unwrap_or(&"Overview")
    }
}

/// State shared by every section of one application editor, provided as context.
#[derive(Clone, Copy)]
struct EditorContext {
    dashboard: StoredValue<super::DashboardContext>,
    saved: RwSignal<ApplicationView>,
    page: StoredValue<Page>,
    /// Environment that runtime actions target: the environment page's, or on
    /// application pages the only environment. `None` with several or none.
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
    /// The manifest the shown environment deploys: the saved one, or when it
    /// follows a branch, the one last fetched from it, read from its loaded
    /// detail. `None` before a branch's first fetch or while the detail loads.
    fn environment_manifest(self) -> Option<ApplicationTemplate> {
        let environment = self.selected_environment()?;
        if environment.source.branch().is_none() {
            return Some(self.saved.with(|saved| saved.application.clone()));
        }
        let signals = self.dashboard.with_value(|d| d.signals);
        signals.detail.with(|detail| {
            let detail = detail
                .as_ref()
                .filter(|detail| detail.environment.id == environment.id)?;
            detail.manifest.clone()
        })
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
        self.saved.with(|saved| saved.deployable(&id).cloned())
    }
    /// Runtime actions require a loaded, live environment, including after
    /// deletion. Background refreshes of an already loaded detail keep them
    /// enabled, so a click during a poll is never swallowed.
    fn environment_action_blocked(self) -> bool {
        let signals = self.dashboard.with_value(|d| d.signals);
        self.action_blocked()
            || self
                .selected_environment()
                .is_none_or(|env| env.delete_intent)
            || signals.detail_error.get().is_some()
            || signals.detail.with(|detail| {
                detail.as_ref().is_none_or(|detail| {
                    Some(detail.environment.id.to_string()) != self.environment.get()
                })
            })
    }
    /// Dashboard address of one of the application's environments.
    fn environment_href(self, environment: &str) -> String {
        format!(
            "/dashboard/applications/{}/environments/{environment}",
            self.id()
        )
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
        let validated = match manifest.validate_template() {
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
impl EditorContext {
    /// Whether this is an environment page rather than the application page.
    fn environment_page(self) -> bool {
        self.page
            .with_value(|page| matches!(page, Page::Environment(_)))
    }

    /// Sends one mutation with a fresh request identity while the editor is
    /// busy, retrying once on transport failure with the same identity so the
    /// server can deduplicate it. Runs `done` with the response, or shows the
    /// failure.
    fn mutate<T: 'static, F: Future<Output = Result<T, ClientError>>>(
        self,
        request: impl Fn(Client) -> F + 'static,
        done: impl FnOnce(T) + 'static,
    ) {
        self.set_error(None);
        let client = match mutation_client() {
            Ok(client) => client,
            Err(error) => {
                self.set_error(Some(error));
                return;
            }
        };
        self.busy.set(true);
        spawn_local(async move {
            let mut result = request(client.clone()).await;
            if result.as_ref().is_err_and(transport_failure) {
                result = request(client).await;
            }
            match result {
                Ok(response) => done(response),
                Err(error) => self.failure(&error),
            }
            self.busy.set(false);
        });
    }

    /// Deploys the saved revision to `environment`, then runs `accepted`.
    fn deploy(self, environment: String, accepted: impl FnOnce() + 'static) {
        let generation = self.saved.with_untracked(|saved| saved.generation);
        self.mutate(
            move |client| {
                let environment = environment.clone();
                async move {
                    client
                        .deploy_environment(&environment, generation, None)
                        .await
                }
            },
            move |_| {
                self.notice
                    .set("Deployment accepted. Follow its progress below.".into());
                accepted();
                self.dashboard.with_value(|d| d.refresh.run(()));
            },
        );
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
    let allowed = super::access::can(piqueld_client::access::Permission::Global(
        piqueld_client::access::GlobalPermission::AppsCreate,
    ));
    view! {
        <Show when={move || allowed.get()}>
            <button type="button" class="btn btn-primary" on:click={move |_| opened.set(true)}>
                {icon(Icon::Plus)}
                "New application"
            </button>
        </Show>
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

/// Loads saved application `id` once and mounts `ApplicationEditor` for `page`.
#[component]
pub(super) fn ApplicationPage(id: String, page: Page) -> impl IntoView {
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
                    view! { <ApplicationEditor initial={initial} page={page.clone()} /> }
                })
        }}
    }
}

/// Provides `EditorContext`, guards navigation while edits are unsaved, and renders
/// the application page, a service editor, or an environment page. The initial
/// tab comes from the `deployment` or `tab` query parameters.
#[component]
fn ApplicationEditor(initial: ApplicationView, page: Page) -> impl IntoView {
    let query = leptos_router::hooks::use_query_map();
    let dashboard = dashboard_context();
    let saved = RwSignal::new(initial);
    let shown = match &page {
        Page::Environment(environment) | Page::Preview(environment) => Some(environment.clone()),
        Page::Application | Page::Service(_) => None,
    };
    let context = EditorContext {
        dashboard: StoredValue::new(dashboard),
        saved,
        environment: Memo::new(move |_| {
            shown.clone().or_else(|| {
                saved.with(|saved| {
                    saved
                        .sole_environment()
                        .ok()
                        .map(|environment| environment.id.to_string())
                })
            })
        }),
        dirty: RwSignal::new(BTreeSet::new()),
        busy: RwSignal::new(false),
        uncertain: RwSignal::new(false),
        error: RwSignal::new(None),
        diagnostic_id: RwSignal::new(None),
        notice: RwSignal::new(String::new()),
        tab: RwSignal::new(query.with_untracked(|query| page.initial_tab(query))),
        page: StoredValue::new(page.clone()),
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
                    saved.environments.clone_from(&application.environments);
                    saved.delete_intent = application.delete_intent;
                });
            }
            // Fetches replace a repository-backed application's saved manifest
            // without advancing its revision; reload it unless it is being
            // edited. Newer revisions show the conflict notice instead.
            let fetched = context.saved.with_untracked(|saved| {
                saved.application.spec().manifest.is_some()
                    && application.generation == saved.generation
                    && application.updated_at_ms > saved.updated_at_ms
            });
            let editing = !context.dirty.get_untracked().is_empty()
                || context.busy.get_untracked()
                || context.uncertain.get_untracked();
            if fetched && !editing {
                let id = application.id.to_string();
                spawn_local(async move {
                    if let Ok(view) = Client::browser().application(&id).await
                        && context.dirty.get_untracked().is_empty()
                    {
                        context.saved.set(view);
                    }
                });
            }
        }
    });
    // The listing has no previews; take them from the detail it reloads.
    Effect::new(move |_| {
        signals.detail.with(|detail| {
            let Some(application) = detail.as_ref().map(|detail| &detail.application) else {
                return;
            };
            if application.application.id().as_str() == context.id()
                && context
                    .saved
                    .with_untracked(|saved| saved.previews != application.previews)
            {
                context
                    .saved
                    .update(|saved| saved.previews.clone_from(&application.previews));
            }
        });
    });
    guard_navigation(context.dirty);
    match page {
        Page::Service(name) => view! { <services::ServiceEditor name={name} /> }.into_any(),
        Page::Environment(_) | Page::Preview(_) => view! { <EnvironmentPage /> }.into_any(),
        Page::Application => view! { <ApplicationSections /> }.into_any(),
    }
}

/// The application page: shared configuration, its environments, and history
/// across all of them.
#[component]
fn ApplicationSections() -> impl IntoView {
    let context = editor();
    let signals = context.dashboard.with_value(|d| d.signals);
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
                <ApplicationOverview />
                <DeleteApplication />
            </div>
        </div>
        <div hidden={move || context.tab.get() != "Environments"}>
            <environments::EnvironmentList />
        </div>
        <Show when={move || context.tab.get() == "Previews"}>
            <previews::PreviewList />
        </Show>
        <ApplicationSettings />
        <Show when={move || context.tab.get() == "Releases"}>
            <releases::ReleaseHistory application={context.id()} />
        </Show>
        <Show when={move || context.tab.get() == "Builds"}>
            <super::builds::BuildHistory application={context.id()} />
        </Show>
        <Show when={move || context.tab.get() == "Events"}>
            <super::observability::EventHistory application={context.id()} />
        </Show>
    }
}

/// Identity and revision of the application.
#[component]
fn ApplicationOverview() -> impl IntoView {
    let context = editor();
    view! {
        <section class="card" aria-labelledby="application-heading">
            <header>
                <div>
                    <h3 id="application-heading">"Application"</h3>
                    <p>
                        "Configuration shared by every environment. Each environment deploys it with its own secrets, volumes, and history."
                    </p>
                </div>
            </header>
            {move || {
                context
                    .saved
                    .with(|saved| {
                        view! {
                            <dl class="kv">
                                <dt>"Application ID"</dt>
                                <dd>
                                    <code>{saved.application.id().to_string()}</code>
                                </dd>
                                <dt>"Configuration"</dt>
                                <dd>
                                    {format!(
                                        "Revision {} · {}",
                                        saved.generation,
                                        if context.managed() {
                                            "managed in Git"
                                        } else {
                                            "saved in piqueld"
                                        },
                                    )}
                                </dd>
                                <dt>"Environments"</dt>
                                <dd>
                                    {saved
                                        .environments
                                        .iter()
                                        .map(|environment| environment.name.to_string())
                                        .collect::<Vec<_>>()
                                        .join(", ")}
                                </dd>
                                <dt>"Created"</dt>
                                <dd>{timestamp(saved.created_at_ms)}</dd>
                                <dt>"Updated"</dt>
                                <dd>{timestamp(saved.updated_at_ms)}</dd>
                            </dl>
                        }
                    })
            }}
        </section>
    }
}

/// One environment or preview: its runtime, deployments, secrets, logs, and
/// history. An environment's overview renames and deletes it; a preview's
/// shows its branch and deletes it.
#[component]
fn EnvironmentPage() -> impl IntoView {
    let context = editor();
    let signals = context.dashboard.with_value(|d| d.signals);
    let id = context.environment_id();
    let preview = context
        .page
        .with_value(|page| matches!(page, Page::Preview(_)));
    let (list, kind) = if preview {
        ("previews", "preview")
    } else {
        ("environments", "environment")
    };
    let app_href = move || format!("/dashboard/applications/{}?tab={list}", context.id());
    if context.selected_environment().is_none() {
        return view! {
            <div class="stack-sm">
                {notice(Tone::Bad, format!("This {kind} no longer exists."))}
                <div class="btn-group">
                    <A attr:class="btn" href={app_href}>
                        {icon(Icon::ArrowLeft)}
                        {format!("Back to {list}")}
                    </A>
                </div>
            </div>
        }
        .into_any();
    }
    let name = move || {
        context
            .selected_environment()
            .map(|environment| environment.name.to_string())
            .unwrap_or_default()
    };
    // Previews are not listed, so their health comes from the loaded detail.
    let health = {
        let id = id.clone();
        move || {
            environment_row(signals, &id)
                .map(|row| row.health())
                .or_else(|| {
                    signals.detail.with(|detail| {
                        detail
                            .as_ref()
                            .filter(|detail| detail.environment.id.as_str() == id)
                            .map(|detail| ApplicationHealth::from_server_state(detail.status.state))
                    })
                })
        }
    };
    let deleting = move || {
        context
            .selected_environment()
            .is_some_and(|environment| environment.delete_intent)
    };
    view! {
        <nav class="breadcrumb" aria-label="Breadcrumb">
            <A href="/dashboard/applications">"Applications"</A>
            {icon(Icon::ChevronRight)}
            <A href={app_href}>{move || context.name()}</A>
            {icon(Icon::ChevronRight)}
            <span>{name}</span>
        </nav>
        <header class="detail-head">
            <div class="detail-title">
                <h1>{name}</h1>
                {move || health().map(health_badge)}
                {move || deleting().then(|| badge(Tone::Warn, "Deleting"))}
            </div>
            <div class="page-actions">
                {if preview {
                    view! { <previews::PreviewActions /> }.into_any()
                } else {
                    view! { <DeploymentActions /> }.into_any()
                }}
            </div>
        </header>
        <EditorFeedback />
        <Tabs label="Environment sections" options={&ENVIRONMENT_TABS} selected={context.tab} />
        <div hidden={move || context.tab.get() != "Overview"}>
            <div class="stack">
                {preview.then(|| view! { <previews::PreviewSettings /> })}
                <RuntimeOverview />
                <variables::EnvironmentVariables />
                {(!preview).then(|| view! { <environments::EnvironmentSettings /> })}
                {preview.then(|| view! { <previews::DeletePreview /> })}
            </div>
        </div>
        <div hidden={move || context.tab.get() != "Deployments"}>
            <DeploymentHistory />
        </div>
        <div hidden={move || context.tab.get() != "Secrets"}>
            <EnvironmentSecrets />
        </div>
        <Show when={move || context.tab.get() == "Logs"}>
            <ApplicationLogs />
        </Show>
        <Show when={move || context.tab.get() == "Events"}>
            <super::observability::EventHistory environment={id.clone()} />
        </Show>
    }
    .into_any()
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
/// notice when the polled listing shows a newer generation than the editor's.
#[component]
fn EditorFeedback() -> impl IntoView {
    let context = editor();
    let signals = dashboard_context().signals;
    let show_status = move || {
        context.busy.get() || (context.dirty.get().is_empty() && !context.notice.get().is_empty())
    };
    let conflict = move || {
        signals.applications.with(|rows| {
            rows.iter().any(|row| {
                row.application.id.as_str() == context.id()
                    && row.application.generation > context.saved.get().generation
            })
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

/// Source, services, routes, volumes, jobs and secret file tabs. Editing them is
/// disabled while the application is managed from a Git manifest.
#[component]
fn ApplicationSettings() -> impl IntoView {
    let context = editor();
    // Forms keep drafts of the saved manifest they were built from. While
    // managed they are read-only, so rebuild them when a fetch changes it.
    let fetched = Memo::new(move |_| {
        context.saved.with(|saved| {
            saved
                .application
                .spec()
                .manifest
                .is_some()
                .then(|| saved.spec_hash.clone())
        })
    });
    view! {
        <div hidden={move || {
            !matches!(
                context.tab.get(),
                "Source" | "Services" | "Variables" | "Routes" | "Volumes" | "Jobs" | "Secrets"
            )
        }}>
            <div class="stack">
                {move || {
                    context
                        .managed()
                        .then(|| {
                            notice(
                                Tone::Info,
                                "Runtime configuration is managed in Git. This page shows the manifest last fetched by any environment; each environment's page shows what it deploys from its own branch. Disconnect the repository in Source to edit services, variables, routes, volumes, jobs, and secret files here.",
                            )
                        })
                }} <div hidden={move || context.tab.get() != "Source"}>
                    <RepositorySettings />
                </div> <fieldset disabled={move || context.managed()}>
                    {move || {
                        // Read, not only tracked: memos are lazy, and one never
                        // read never subscribes to `saved`.
                        fetched.with(|_| ());
                        view! {
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
                            <div hidden={move || context.tab.get() != "Variables"}>
                                <variables::VariableSettings />
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
                            <div hidden={move || context.tab.get() != "Secrets"}>
                                <SecretFileSettings />
                            </div>
                        }
                    }}
                </fieldset>
            </div>
        </div>
    }
}
