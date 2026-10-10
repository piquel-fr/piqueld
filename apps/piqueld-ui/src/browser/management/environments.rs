//! Environment lifecycle controls and where an environment deploys from (the
//! branch it follows, or the environment it is promoted from) and whether
//! pushes to its branch deploy it; configuration continues to belong to the
//! application or its repository.
use super::super::environment_row;
use super::super::format::commit;
use super::super::ui::{
    Icon, Modal, Tone, badge, empty, health_badge, icon, notice, operation_badge, text_input, when,
};
use super::deployments::PromoteAction;
use super::{EditorContext, editor, mutation_client, transport_failure};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::NavigateOptions;
use leptos_router::components::A;
use leptos_router::hooks::use_navigate;
use piqueld_client::{
    Client, ClientError, CreateEnvironmentRequest, EnvironmentBranchRequest, EnvironmentName,
    EnvironmentRequest, EnvironmentSourceRequest, EnvironmentView, TrackedBranch, Visibility,
    edit::ApplicationEdit,
    sync::{SyncState, SyncedHead},
};

enum EnvironmentChange {
    /// `promote_from` excludes `branch`.
    Create {
        name: String,
        branch: Option<TrackedBranch>,
        promote_from: Option<String>,
    },
    Rename {
        id: String,
        name: String,
    },
    Branch {
        id: String,
        branch: TrackedBranch,
    },
    /// Opts in or out of the application's sync, without a revision.
    Sync {
        id: String,
        enabled: bool,
    },
    /// Promoted from the given environment, or `None` to track its own source.
    Source {
        id: String,
        promote_from: Option<String>,
    },
    Delete(String),
    RetryDeletion(String),
}

impl EnvironmentChange {
    async fn send(
        &self,
        client: &Client,
        application: &str,
        generation: u64,
    ) -> Result<Option<EnvironmentView>, ClientError> {
        let expected_generation = Some(generation);
        match self {
            Self::Create {
                name,
                branch,
                promote_from,
            } => {
                let request = CreateEnvironmentRequest {
                    name: name.clone(),
                    branch: branch.as_ref().map(|branch| branch.branch().to_owned()),
                    commit: branch
                        .as_ref()
                        .and_then(|branch| branch.commit().map(str::to_owned)),
                    promote_from: promote_from.clone(),
                    expected_generation,
                };
                client
                    .create_environment(application, &request, false)
                    .await
                    .map(Some)
            }
            Self::Rename { id, name } => {
                let request = EnvironmentRequest {
                    name: name.clone(),
                    expected_generation,
                };
                client
                    .rename_environment(id, &request, false)
                    .await
                    .map(Some)
            }
            Self::Branch { id, branch } => {
                let request = EnvironmentBranchRequest {
                    branch: branch.branch().to_owned(),
                    commit: branch.commit().map(str::to_owned),
                    expected_generation,
                };
                client
                    .set_environment_branch(id, &request, false)
                    .await
                    .map(Some)
            }
            Self::Sync { id, enabled } => client.set_environment_sync(id, *enabled).await.map(Some),
            Self::Source { id, promote_from } => {
                let request = EnvironmentSourceRequest {
                    promote_from: promote_from.clone(),
                    expected_generation,
                };
                client
                    .set_environment_source(id, &request, false)
                    .await
                    .map(Some)
            }
            Self::Delete(id) => client
                .delete_environment(id, Some(generation), false)
                .await
                .map(|_| None),
            Self::RetryDeletion(id) => client
                .reconcile_environment(id, Some(generation))
                .await
                .map(|_| None),
        }
    }
}

impl EditorContext {
    /// Retries ambiguous transport failures with the same request identity and
    /// updates only environment metadata, preserving application form drafts.
    fn change_environment(
        self,
        change: EnvironmentChange,
        done: impl FnOnce(Option<EnvironmentView>) + 'static,
    ) {
        if self.action_blocked() {
            return;
        }
        let client = match mutation_client() {
            Ok(client) => client,
            Err(error) => {
                self.set_error(Some(error));
                return;
            }
        };
        let saved = self.saved.get_untracked();
        self.set_error(None);
        self.busy.set(true);
        spawn_local(async move {
            let id = saved.application.id().as_str();
            let mut result = change.send(&client, id, saved.generation).await;
            if result.as_ref().is_err_and(transport_failure) {
                result = change.send(&client, id, saved.generation).await;
            }
            match result {
                Ok(environment) => {
                    self.saved.update(|saved| {
                        // Environment lifecycle changes advance the application
                        // revision, so later edits stay guarded by it.
                        if !matches!(
                            change,
                            EnvironmentChange::RetryDeletion(_) | EnvironmentChange::Sync { .. }
                        ) {
                            saved.generation += 1;
                        }
                        if let Some(environment) = &environment {
                            saved.environments.retain(|env| env.id != environment.id);
                            saved.environments.push(environment.clone());
                            saved.environments.sort_by(|a, b| a.name.cmp(&b.name));
                        } else if let EnvironmentChange::Delete(id) = &change
                            && let Some(env) = saved
                                .environments
                                .iter_mut()
                                .find(|env| env.id.as_str() == id)
                        {
                            env.delete_intent = true;
                        }
                    });
                    self.notice.set(if environment.is_some() {
                        "Environment saved.".into()
                    } else {
                        "Environment deletion accepted. Its volumes will be retained.".into()
                    });
                    done(environment);
                    self.dashboard.with_value(|d| d.refresh.run(()));
                }
                Err(error) => self.failure(&error),
            }
            self.busy.set(false);
        });
    }
}

/// The application's environments with their health and latest deployment.
/// Each whole row opens the environment's page; its Deploy button deploys it,
/// or for a promoted environment its Promote button promotes into it. Also
/// creates new environments.
#[component]
pub(super) fn EnvironmentList() -> impl IntoView {
    let context = editor();
    let signals = context.dashboard.with_value(|d| d.signals);
    let navigate = use_navigate();
    // Latest deployment of an environment among the listed deployments.
    let latest = move |environment: &str| {
        signals.applications.with(|rows| {
            rows.iter()
                .flat_map(|row| &row.deployments)
                .filter(|deployment| deployment.operation.environment_id.as_str() == environment)
                .max_by_key(|deployment| deployment.operation.created_at_ms)
                .map(|deployment| deployment.operation.clone())
        })
    };
    view! {
        <div class="stack">
            <div class="section-header">
                <div>
                    <h2>"Environments"</h2>
                    <p>
                        "Every environment deploys the application's configuration with its own secrets, volumes, and history."
                    </p>
                </div>
                <NewEnvironment />
            </div>
            {move || {
                let environments = context.saved.with(|saved| saved.environments.clone());
                if environments.is_empty() {
                    return empty("No environments yet. Create one to deploy this application.");
                }
                let navigate = navigate.clone();
                view! {
                    <ul class="list" aria-label="Environments">
                        {environments
                            .into_iter()
                            .map(|environment| {
                                let id = environment.id.to_string();
                                let href = context.environment_href(&id);
                                let navigate = navigate.clone();
                                let deleting = environment.delete_intent;
                                let name = environment.name.to_string();
                                let source = context.describe_source(&environment.source);
                                let promoted = environment.source.promoted_from().is_some();
                                let label = format!("Deploy to {name}");
                                let health = environment_row(signals, &id).map(|row| row.health());
                                let latest = latest(&id);
                                view! {
                                    <li class="list-row list-row-link">
                                        <span class="title">
                                            <A href={href}>{name}</A>
                                            <small>{format!("{id} · {source}")}</small>
                                        </span>
                                        <span class="meta">
                                            {latest
                                                .map_or_else(
                                                    || "Never deployed".into_any(),
                                                    |operation| {
                                                        view! {
                                                            <span class="btn-group">
                                                                {operation_badge(operation.state)}
                                                                {when(operation.created_at_ms)}
                                                            </span>
                                                        }
                                                            .into_any()
                                                    },
                                                )}
                                        </span>
                                        {if deleting {
                                            badge(Tone::Warn, "Deleting")
                                        } else {
                                            health.map(health_badge).into_any()
                                        }}
                                        {if promoted {
                                            view! { <PromoteAction environment={id} compact=true /> }
                                                .into_any()
                                        } else {
                                            view! {
                                                <button
                                                    type="button"
                                                    class="btn btn-sm"
                                                    aria-label={label}
                                                    disabled={move || context.action_blocked() || deleting}
                                                    on:click={move |_| {
                                                        let navigate = navigate.clone();
                                                        let shown = id.clone();
                                                        context
                                                            .deploy(
                                                                id.clone(),
                                                                move || context.show_deployments(&shown, navigate),
                                                            );
                                                    }}
                                                >
                                                    {icon(Icon::Rocket)}
                                                    "Deploy"
                                                </button>
                                            }
                                                .into_any()
                                        }}
                                        <span class="chevron" aria-hidden="true">
                                            {icon(Icon::ChevronRight)}
                                        </span>
                                    </li>
                                }
                            })
                            .collect_view()}
                    </ul>
                }
                    .into_any()
            }}
        </div>
    }
}

/// Badge for whether pushes deploy an environment or preview, with why or why not.
pub(super) fn sync_badge(state: SyncState) -> (AnyView, &'static str) {
    let (tone, reason) = match state {
        SyncState::Following => (Tone::Ok, "Pushes to its branch deploy it."),
        SyncState::Off => (
            Tone::Neutral,
            "Its application does not deploy on push; turn it on in the application's Source tab.",
        ),
        SyncState::Pinned => (
            Tone::Neutral,
            "It is pinned to a commit, so pushes never move it.",
        ),
        SyncState::NotOptedIn => (
            Tone::Neutral,
            "It has not opted in: pushes to its branch do not deploy it.",
        ),
        SyncState::AwaitingDeployment => (
            Tone::Warn,
            "It follows its branch from its next deployment, since it was never deployed or its branch or repository changed.",
        ),
    };
    (badge(tone, state.to_string()), reason)
}

/// The branch head as of the last deployment, with the full commit on hover.
pub(super) fn last_synced(synced: Option<SyncedHead>) -> AnyView {
    synced.map_or_else(
        || view! { <span class="muted">"Never"</span> }.into_any(),
        |head| {
            view! {
                <code title={head.commit.clone()}>{commit(&head.commit).to_owned()}</code>
                " · "
                {when(head.at_ms)}
            }
            .into_any()
        },
    )
}

/// Validates a branch and optional commit typed into a form; a blank commit
/// follows the branch head.
fn tracked_branch((branch, commit): (String, String)) -> Result<TrackedBranch, String> {
    let commit = (!commit.trim().is_empty()).then(|| commit.trim().to_owned());
    TrackedBranch::new(branch.trim().to_owned(), commit).map_err(|error| error.to_string())
}

/// The branch and commit a repository-backed application's new environments
/// follow by default, from its `spec.manifest`.
fn default_branch(context: super::EditorContext) -> (String, String) {
    context
        .manifest()
        .spec
        .manifest
        .map(|manifest| {
            (
                manifest.repository.branch,
                manifest.repository.commit.unwrap_or_default(),
            )
        })
        .unwrap_or_default()
}

impl EditorContext {
    /// IDs and names of the environments other than `exclude` that another
    /// can be promoted from: the application's live, non-preview ones.
    fn promotion_sources(self, exclude: &str) -> Vec<(String, String)> {
        self.saved.with(|saved| {
            saved
                .environments
                .iter()
                .filter(|env| env.id.as_str() != exclude && !env.delete_intent)
                .map(|env| (env.id.to_string(), env.name.to_string()))
                .collect()
        })
    }
}

/// "Promote from" select over `sources`; its value is the chosen
/// environment's ID, or empty for the `none` option.
#[component]
fn PromotionSource(
    value: RwSignal<String>,
    sources: Signal<Vec<(String, String)>>,
    none: &'static str,
) -> impl IntoView {
    view! {
        <label class="field">
            <span>"Promote from"</span>
            <select on:change={move |event| value.set(event_target_value(&event))}>
                <option value="" prop:selected={move || value.with(String::is_empty)}>
                    {none}
                </option>
                {move || {
                    sources
                        .get()
                        .into_iter()
                        .map(|(id, name)| {
                            let selected = id.clone();
                            view! {
                                <option
                                    value={id}
                                    prop:selected={move || value.with(|value| *value == selected)}
                                >
                                    {name}
                                </option>
                            }
                        })
                        .collect_view()
                }}
            </select>
        </label>
    }
}

/// "New environment" button and dialog; opens the created environment. It
/// builds its own source, choosing the branch to follow for a
/// repository-backed application, or is promoted from another environment.
#[component]
fn NewEnvironment() -> impl IntoView {
    let context = editor();
    let opened = RwSignal::new(false);
    let name = RwSignal::new(String::new());
    let branch = RwSignal::new(default_branch(context));
    // The environment to promote from; empty to build its own source.
    let promote_from = RwSignal::new(String::new());
    let builds = move || promote_from.with(String::is_empty);
    let sources = Signal::derive(move || context.promotion_sources(""));
    let navigate = use_navigate();
    let submit = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        let value = name.get_untracked();
        if let Err(error) = EnvironmentName::parse(&value) {
            context.set_error(Some(error.to_string()));
            return;
        }
        let tracked = if context.managed() && builds() {
            match tracked_branch(branch.get_untracked()) {
                Ok(tracked) => Some(tracked),
                Err(error) => {
                    context.set_error(Some(error));
                    return;
                }
            }
        } else {
            None
        };
        let navigate = navigate.clone();
        let change = EnvironmentChange::Create {
            name: value,
            branch: tracked,
            promote_from: Some(promote_from.get_untracked()).filter(|id| !id.is_empty()),
        };
        context.change_environment(change, move |environment| {
            opened.set(false);
            if let Some(environment) = environment {
                navigate(
                    &context.environment_href(environment.id.as_str()),
                    NavigateOptions::default(),
                );
            }
        });
    };
    view! {
        <button
            type="button"
            class="btn btn-primary"
            disabled={move || context.action_blocked()}
            on:click={move |_| {
                name.set(String::new());
                branch.set(default_branch(context));
                promote_from.set(String::new());
                opened.set(true);
            }}
        >
            {icon(Icon::Plus)}
            "New environment"
        </button>
        <Modal title="Create environment" opened={opened} busy={context.busy}>
            <form class="stack-sm" on:submit={submit}>
                <fieldset class="stack-sm" disabled={move || context.action_blocked()}>
                    <p class="hint">
                        {move || {
                            if !builds() {
                                "The environment starts empty and only deploys releases promoted from the chosen environment, with its own secrets, volumes, and history."
                            } else if context.managed() {
                                "The environment starts empty and deploys the manifest on its own branch of the application's repository, with its own secrets, volumes, and history."
                            } else {
                                "The environment starts empty and deploys the application's configuration with its own secrets, volumes, and history."
                            }
                        }}
                    </p>
                    {text_input("Environment name", name, String::clone, |value, input| *value = input)}
                    <Show when={move || sources.with(|sources| !sources.is_empty())}>
                        <PromotionSource
                            value={promote_from}
                            sources={sources}
                            none="None: build its own source"
                        />
                    </Show>
                    <Show when={move || context.managed() && builds()}>
                        {text_input("Branch", branch, |v| v.0.clone(), |v, input| v.0 = input)}
                        {text_input(
                            "Commit (optional)",
                            branch,
                            |v| v.1.clone(),
                            |v, input| v.1 = input,
                        )}
                    </Show>
                </fieldset>
                {move || context.error.get().map(|error| notice(Tone::Bad, error))}
                <div class="form-actions">
                    <button
                        type="submit"
                        class="btn btn-primary"
                        disabled={move || context.action_blocked()}
                    >
                        "Create environment"
                    </button>
                </div>
            </form>
        </Modal>
    }
}

/// Where the environment page's environment deploys from. A tracking one
/// shows the branch it follows, with a form to point it at another branch or
/// pin a commit (one deploying the saved manifest only says so), and whether
/// pushes to it deploy it, with a button to opt in or out; a promoted one
/// links its source and can track its own source again. Either can be made
/// promoted from another environment.
#[component]
fn SourceSettings() -> impl IntoView {
    let context = editor();
    let id = StoredValue::new(context.environment_id());
    let source = Memo::new(move |_| {
        context
            .selected_environment()
            .map(|environment| environment.source)
    });
    let current =
        move || source.with(|source| source.as_ref().and_then(|source| source.branch().cloned()));
    let promoted = move || {
        source.with(|source| {
            source
                .as_ref()
                .and_then(|source| source.promoted_from())
                .map(ToString::to_string)
        })
    };
    let drafts = move || {
        let branch = current().map_or_else(Default::default, |branch| {
            (
                branch.branch().to_owned(),
                branch.commit().unwrap_or_default().to_owned(),
            )
        });
        (branch, promoted().unwrap_or_default())
    };
    let (initial_branch, initial_source) = drafts();
    let branch = RwSignal::new(initial_branch);
    let promote_from = RwSignal::new(initial_source);
    // Follow source changes, e.g. a promoted environment tracking again.
    Effect::new(move |_| {
        let (current_branch, current_source) = drafts();
        branch.set(current_branch);
        promote_from.set(current_source);
    });
    let sources = Signal::derive(move || context.promotion_sources(&id.get_value()));
    let deleting = move || {
        context
            .selected_environment()
            .is_some_and(|environment| environment.delete_intent)
    };
    let blocked = move || context.action_blocked() || deleting();
    let save = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        match tracked_branch(branch.get_untracked()) {
            Ok(tracked) => context.change_environment(
                EnvironmentChange::Branch {
                    id: id.get_value(),
                    branch: tracked,
                },
                |_| {},
            ),
            Err(error) => context.set_error(Some(error)),
        }
    };
    let sync = move || {
        let environment = context.selected_environment()?;
        let state = context
            .saved
            .with(|saved| environment.sync_state(saved.application.spec().manifest.as_ref()));
        Some((environment, state))
    };
    let toggle_sync = move |_| {
        if let Some((environment, _)) = sync() {
            let change = EnvironmentChange::Sync {
                id: id.get_value(),
                enabled: !environment.sync,
            };
            context.change_environment(change, |_| {});
        }
    };
    let change_source = move |promote_from: Option<String>| {
        context.change_environment(
            EnvironmentChange::Source {
                id: id.get_value(),
                promote_from,
            },
            |_| {},
        );
    };
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>"Source"</h3>
                    <p>
                        {move || match promoted() {
                            Some(from) => {
                                view! {
                                    "Receives releases promoted from "
                                    <A href={context.environment_href(&from)}>
                                        {context.environment_name(&from)}
                                    </A>
                                    "; it never builds or fetches. Tracking its own source again builds the saved manifest, or the branch the application's manifest names; nothing is redeployed."
                                }
                                    .into_any()
                            }
                            None if current().is_some() => {
                                "Deploys the manifest on this branch of the application's repository. Changing it redeploys nothing; the next deployment fetches the new branch."
                                    .into_any()
                            }
                            None => "Deploys the application's saved manifest.".into_any(),
                        }}
                    </p>
                </div>
                <Show when={move || promoted().is_some()}>
                    <button
                        type="button"
                        class="btn"
                        disabled={blocked}
                        on:click={move |_| change_source(None)}
                    >
                        "Track own source"
                    </button>
                </Show>
            </header>
            <Show when={move || current().is_some()}>
                <form class="stack-sm" on:submit={save}>
                    <fieldset class="form-grid" disabled={blocked}>
                        {text_input("Branch", branch, |v| v.0.clone(), |v, input| v.0 = input)}
                        {text_input(
                            "Commit (optional)",
                            branch,
                            |v| v.1.clone(),
                            |v, input| v.1 = input,
                        )}
                    </fieldset>
                    <div class="form-actions">
                        <button
                            type="submit"
                            class="btn"
                            disabled={move || {
                                blocked() || tracked_branch(branch.get()).ok() == current()
                            }}
                        >
                            "Change branch"
                        </button>
                    </div>
                </form>
                {move || {
                    sync()
                        .map(|(environment, state)| {
                            let (badge, reason) = sync_badge(state);
                            view! {
                                <dl class="kv">
                                    <dt>"Deploy on push"</dt>
                                    <dd>
                                        {badge}
                                        <div class="muted">{reason}</div>
                                    </dd>
                                    <dt>"Last synced"</dt>
                                    <dd>{last_synced(environment.synced)}</dd>
                                </dl>
                                <div class="form-actions">
                                    <button
                                        type="button"
                                        class="btn"
                                        disabled={move || context.action_blocked() || deleting()}
                                        on:click={toggle_sync}
                                    >
                                        {if environment.sync {
                                            "Stop deploying on push"
                                        } else {
                                            "Deploy on push"
                                        }}
                                    </button>
                                </div>
                            }
                        })
                }}
            </Show>
            <Show when={move || sources.with(|sources| !sources.is_empty())}>
                <form
                    class="stack-sm"
                    on:submit={move |event: leptos::ev::SubmitEvent| {
                        event.prevent_default();
                        change_source(Some(promote_from.get_untracked()));
                    }}
                >
                    <p class="hint">
                        "A promoted environment never builds: it only deploys releases promoted from its source environment. Changing the source redeploys nothing."
                    </p>
                    <fieldset class="stack-sm" disabled={blocked}>
                        <PromotionSource
                            value={promote_from}
                            sources={sources}
                            none="Choose an environment"
                        />
                    </fieldset>
                    <div class="form-actions">
                        <button
                            type="submit"
                            class="btn"
                            disabled={move || {
                                blocked() || promote_from.with(String::is_empty)
                                    || Some(promote_from.get()) == promoted()
                            }}
                        >
                            {move || {
                                if promoted().is_some() {
                                    "Change source environment"
                                } else {
                                    "Promote from environment"
                                }
                            }}
                        </button>
                    </div>
                </form>
            </Show>
        </section>
    }
}

/// The visibility ceiling of the environment page's environment, from the
/// manifest it deploys and saved in `[spec.environments.<name>]`. Read-only
/// while the application is managed in Git.
#[component]
fn EnvironmentVisibility() -> impl IntoView {
    let context = editor();
    let name = move || {
        context
            .selected_environment()
            .map(|environment| environment.name.to_string())
            .unwrap_or_default()
    };
    let current = move || {
        context
            .environment_manifest()
            .and_then(|manifest| {
                let name = name();
                manifest
                    .spec()
                    .environments
                    .get(&name)
                    .map(|config| config.visibility)
            })
            .unwrap_or(Visibility::Public)
    };
    let draft = RwSignal::new(current());
    // Follow saves and fetches.
    Effect::new(move |_| draft.set(current()));
    let save = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        context.save(
            ApplicationEdit::EnvironmentVisibility {
                environment: name(),
                visibility: draft.get_untracked(),
            },
            Callback::new(|_| {}),
        );
    };
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>"Visibility"</h3>
                    <p>
                        "The strictest visibility this environment's routes get. Private keeps every route on the tailnet, whatever the route itself says; public leaves each route's own visibility. Save, then deploy to apply it."
                    </p>
                </div>
            </header>
            <form class="stack-sm" on:submit={save}>
                <fieldset class="stack-sm" disabled={move || context.blocked() || context.managed()}>
                    <label class="field">
                        <span>"Route visibility"</span>
                        <select
                            prop:value={move || draft.get().to_string()}
                            on:change={move |event| {
                                if let Ok(visibility) = event_target_value(&event).parse() {
                                    draft.set(visibility);
                                }
                            }}
                        >
                            <option value="public">"Public: routes keep their own visibility"</option>
                            <option value="private">"Private: every route is tailnet-only"</option>
                        </select>
                    </label>
                </fieldset>
                <div class="form-actions">
                    <button
                        type="submit"
                        class="btn"
                        disabled={move || {
                            context.blocked() || context.managed() || draft.get() == current()
                        }}
                    >
                        "Save visibility"
                    </button>
                </div>
            </form>
        </section>
    }
}

/// Source, visibility, rename and deletion of the environment page's
/// environment. Deletion returns to the application's environments once
/// accepted.
#[component]
pub(super) fn EnvironmentSettings() -> impl IntoView {
    let context = editor();
    let navigate = use_navigate();
    let id = StoredValue::new(context.environment_id());
    let current = move || {
        context
            .selected_environment()
            .map(|environment| environment.name.to_string())
            .unwrap_or_default()
    };
    let name = RwSignal::new(current());
    let deleting = move || {
        context
            .selected_environment()
            .is_some_and(|environment| environment.delete_intent)
    };
    let rename = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        let value = name.get_untracked();
        if let Err(error) = EnvironmentName::parse(&value) {
            context.set_error(Some(error.to_string()));
            return;
        }
        context.change_environment(
            EnvironmentChange::Rename {
                id: id.get_value(),
                name: value,
            },
            |_| {},
        );
    };
    let delete = move |_| {
        if deleting() {
            context.change_environment(EnvironmentChange::RetryDeletion(id.get_value()), |_| {});
            return;
        }
        let message = format!(
            "Delete environment {}, its services, secrets and deployment history? The application and other environments remain. Docker volume data will be retained.",
            current()
        );
        if !window().confirm_with_message(&message).unwrap_or(false) {
            return;
        }
        let navigate = navigate.clone();
        let href = format!("/dashboard/applications/{}?tab=environments", context.id());
        context.change_environment(EnvironmentChange::Delete(id.get_value()), move |_| {
            navigate(&href, NavigateOptions::default());
        });
    };
    view! {
        <SourceSettings />
        <EnvironmentVisibility />
        <section class="card">
            <header>
                <div>
                    <h3>"Environment"</h3>
                    <p>"Renaming changes only the name; nothing is redeployed."</p>
                </div>
            </header>
            <form class="stack-sm" on:submit={rename}>
                <fieldset class="stack-sm" disabled={move || context.action_blocked() || deleting()}>
                    {text_input("Environment name", name, String::clone, |value, input| *value = input)}
                    <dl class="kv">
                        <dt>"Environment ID"</dt>
                        <dd>
                            <code>{id.get_value()}</code>
                        </dd>
                    </dl>
                </fieldset>
                <div class="form-actions">
                    <button
                        type="submit"
                        class="btn"
                        disabled={move || {
                            context.action_blocked() || deleting() || name.get() == current()
                        }}
                    >
                        "Rename environment"
                    </button>
                </div>
            </form>
        </section>
        <section class="card card-danger">
            <header>
                <div>
                    <h3>"Delete environment"</h3>
                    <p>
                        "Removes this environment's running services, secrets, and deployment history. Its events stay in the application's history. Docker volumes and their data are retained."
                    </p>
                </div>
                <button
                    type="button"
                    class="btn btn-danger"
                    disabled={move || context.action_blocked()}
                    on:click={delete}
                >
                    {move || if deleting() { "Retry deletion" } else { "Delete environment" }}
                </button>
            </header>
        </section>
    }
}
