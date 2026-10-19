//! Environment lifecycle controls and where an environment deploys from (the
//! branch it follows, or the environment it is promoted from) and whether
//! pushes to its branch deploy it; configuration continues to belong to the
//! application or its repository. The manifest decides whether environments
//! it declares exist, and their source and sync when their block sets them,
//! so those controls are then read-only.
use super::super::access::can;
use super::super::confirm;
use super::super::ui::{
    Icon, Modal, Tone, badge, empty, health_badge, icon, notice, operation_badge, text_field,
    text_input, when,
};
use super::deployments::EnvironmentAction;
use super::{Draft, EditorContext, editor};
use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::components::A;
use leptos_router::hooks::use_navigate;
use piqueld_client::sync::short_commit;
use piqueld_client::{
    ApplicationManifest, ApplicationSpec, Client, ClientError, CreateEnvironmentRequest,
    EnvironmentBranchRequest, EnvironmentChange, EnvironmentName, EnvironmentRequest,
    EnvironmentSourceRequest, EnvironmentView, TrackedBranch, Visibility,
    access::{GlobalPermission, Permission},
    edit::ApplicationEdit,
    sync::{SyncState, SyncedHead},
};

/// A change to one of the application's environments. All but `Sync` are
/// guarded by the saved generation.
#[derive(Clone)]
enum EnvironmentEdit {
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
    /// Grants or revokes the daemon's telemetry, without a revision.
    Telemetry {
        id: String,
        granted: bool,
    },
    /// Promoted from the given environment, or `None` to track its own source.
    Source {
        id: String,
        promote_from: Option<String>,
    },
    /// Deletes the environment, retaining its volumes.
    Delete(String),
    /// Retries a deletion that did not finish.
    RetryDeletion(String),
}

/// Dirty group of the environment page's source form.
const SOURCE_FORM: &str = "environment-source";
/// Dirty group of the environment page's rename form.
const NAME_FORM: &str = "environment-name";

impl EnvironmentEdit {
    /// Whether a replay after a transport failure is safe. Deletions are not
    /// retried, as they could fail once the first attempt completed.
    const fn replayable(&self) -> bool {
        !matches!(self, Self::Delete(_) | Self::RetryDeletion(_))
    }

    /// The form whose unsaved edits the change saves, if any.
    const fn form(&self) -> Option<&'static str> {
        match self {
            Self::Rename { .. } => Some(NAME_FORM),
            Self::Branch { .. } | Self::Source { .. } => Some(SOURCE_FORM),
            _ => None,
        }
    }

    /// Sends the change, returning the changed environment unless it is
    /// being deleted.
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
            Self::Telemetry { id, granted } => client
                .set_environment_telemetry(id, *granted)
                .await
                .map(Some),
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
    /// Sends `change` (see `mutate`, or `mutate_once` unless it is
    /// replayable) and updates only environment metadata, preserving
    /// application form drafts.
    fn change_environment(
        self,
        change: EnvironmentEdit,
        done: impl FnOnce(Option<EnvironmentView>) + 'static,
    ) {
        if self.change_blocked(change.form()) {
            return;
        }
        let (application, generation) = self
            .saved
            .with_untracked(|saved| (saved.application.id().to_string(), saved.generation));
        // Environment lifecycle changes advance the application revision, so
        // later edits stay guarded by it.
        let advances = !matches!(
            change,
            EnvironmentEdit::RetryDeletion(_)
                | EnvironmentEdit::Sync { .. }
                | EnvironmentEdit::Telemetry { .. }
        );
        let deleted = match &change {
            EnvironmentEdit::Delete(id) => Some(id.clone()),
            _ => None,
        };
        let replayable = change.replayable();
        let send = move |client: Client| {
            let (change, application) = (change.clone(), application.clone());
            async move { change.send(&client, &application, generation).await }
        };
        let applied = move |environment: Option<EnvironmentView>| {
            self.saved.update(|saved| {
                if advances {
                    saved.generation += 1;
                }
                if let Some(environment) = &environment {
                    saved.environments.retain(|env| env.id != environment.id);
                    saved.environments.push(environment.clone());
                    saved.environments.sort_by(|a, b| a.name.cmp(&b.name));
                } else if let Some(env) = saved
                    .environments
                    .iter_mut()
                    .find(|env| deleted.as_deref() == Some(env.id.as_str()))
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
            self.refresh_dashboard();
        };
        if replayable {
            self.mutate(send, applied);
        } else {
            self.mutate_once(async move |client| send(client).await, applied);
        }
    }
}

/// The application's environments with their health and latest deployment.
/// Each whole row opens the environment's page; its Deploy button deploys it,
/// or for a promoted environment its Promote button promotes into it. Also
/// creates new environments.
#[component]
pub(super) fn EnvironmentList() -> impl IntoView {
    let context = editor();
    let signals = context.signals();
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
            <Drift />
            {move || {
                let environments = context.saved.with(|saved| saved.environments.clone());
                if environments.is_empty() {
                    return empty("No environments yet. Create one to deploy this application.");
                }
                view! {
                    <ul class="list" aria-label="Environments">
                        {environments
                            .into_iter()
                            .map(|environment| {
                                let id = environment.id.to_string();
                                let href = context.environment_href(&id);
                                let deleting = environment.delete_intent;
                                let name = environment.name.to_string();
                                let source = context.describe_source(&environment.source);
                                let health = signals.environment_row(&id).map(|row| row.health());
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
                                        <EnvironmentAction environment={environment} compact=true />
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

/// How the environments differ from the ones the saved manifest declares:
/// changes waiting for its next save or fetch, and environments it no longer
/// declares, which stay until deleted. Hidden when they match.
#[component]
fn Drift() -> impl IntoView {
    let context = editor();
    move || {
        let drift = context.saved.with(|saved| saved.drift.clone());
        (!drift.is_empty()).then(|| {
            view! {
                <section class="card" aria-label="Differences from the manifest">
                    <header>
                        <div>
                            <h3>"Differs from the manifest"</h3>
                            <p>
                                "The manifest declares this application's environments. These differences apply at its next save or fetch, except environments it no longer declares, which keep running until you delete them."
                            </p>
                        </div>
                    </header>
                    <ul class="list">
                        {drift
                            .into_iter()
                            .map(|change| {
                                let (tone, label) = match change {
                                    EnvironmentChange::Undeclared { .. } => {
                                        (Tone::Warn, "Not declared")
                                    }
                                    _ => (Tone::Neutral, "Pending"),
                                };
                                view! {
                                    <li class="list-row">
                                        <span class="title">
                                            {change.environment().to_string()}
                                            <small>{change.summary()}</small>
                                        </span>
                                        {badge(tone, label)}
                                    </li>
                                }
                            })
                            .collect_view()}
                    </ul>
                </section>
            }
        })
    }
}

/// Explains that the manifest owns the environment's source and sync, so
/// the forms that would change them are read-only.
fn declared_notice(name: &str) -> AnyView {
    notice(
        Tone::Neutral,
        format!(
            "The manifest sets this environment's source in [spec.environments.{name}], so its source and deploy on push are changed there."
        ),
    )
    .into_any()
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

/// The branch head as of the last deployment, with the full commit on hover;
/// "Not pushed" when a local snapshot was deployed before its branch existed.
pub(super) fn last_synced(synced: Option<SyncedHead>) -> AnyView {
    synced.map_or_else(
        || view! { <span class="muted">"Never"</span> }.into_any(),
        |head| {
            let head_view = match head.commit {
                Some(head) => {
                    view! { <code title={head.clone()}>{short_commit(&head).to_owned()}</code> }
                        .into_any()
                }
                None => view! { <span class="muted">"Not pushed"</span> }.into_any(),
            };
            view! {
                {head_view}
                " · "
                {when(head.at_ms)}
            }
            .into_any()
        },
    )
}

/// Branch and optional commit inputs of `branch`.
fn branch_fields(branch: RwSignal<(String, String)>) -> impl IntoView {
    view! {
        {text_input("Branch", branch, |v| v.0.clone(), |v, input| v.0 = input)}
        {text_input("Commit (optional)", branch, |v| v.1.clone(), |v, input| v.1 = input)}
    }
}

/// Validates a branch and optional commit typed into a form; a blank commit
/// follows the branch head.
fn tracked_branch((branch, commit): (String, String)) -> Result<TrackedBranch, String> {
    let commit = (!commit.trim().is_empty()).then(|| commit.trim().to_owned());
    TrackedBranch::new(branch.trim().to_owned(), commit).map_err(|error| error.to_string())
}

/// The branch and commit a repository-backed application's new environments
/// follow by default, from its `spec.manifest`.
fn default_branch(context: EditorContext) -> (String, String) {
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

/// A `label`led ("Promote from" by default) select over `sources`, each
/// option named after `prefix`; its value is the chosen environment's ID,
/// or empty for the `none` option.
#[component]
fn PromotionSource(
    value: RwSignal<String>,
    sources: Signal<Vec<(String, String)>>,
    none: &'static str,
    #[prop(default = "Promote from")] label: &'static str,
    #[prop(default = "")] prefix: &'static str,
) -> impl IntoView {
    view! {
        <label class="field">
            <span>{label}</span>
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
                                    {format!("{prefix}{name}")}
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
        let change = EnvironmentEdit::Create {
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
                    <Show when={move || {
                        context
                            .saved
                            .with(|saved| !saved.application.spec().environments.is_empty())
                    }}>
                        {notice(
                            Tone::Neutral,
                            "The manifest declares this application's environments. To add one it manages, add a [spec.environments.<name>] block to it; one created here is not declared, and shows as a difference from the manifest.",
                        )}
                    </Show>
                    {text_field("Environment name", name)}
                    <Show when={move || sources.with(|sources| !sources.is_empty())}>
                        <PromotionSource
                            value={promote_from}
                            sources={sources}
                            none="None: build its own source"
                        />
                    </Show>
                    <Show when={move || {
                        context.managed() && builds()
                    }}>{branch_fields(branch)}</Show>
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

/// Where the environment page's environment deploys from, as one choice:
/// building its own source (the saved manifest, or a branch and optional
/// commit of the application's repository) or receiving releases promoted
/// from another environment. Saving changes only where later deployments
/// come from; nothing is redeployed. An environment following a branch also
/// shows whether pushes to it deploy it, with a button to opt in or out.
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
    let repository = move || {
        context
            .saved
            .with(|saved| saved.application.spec().manifest.is_some())
    };
    // The choice as currently saved: the branch it follows (by default the
    // one `spec.manifest` names) and the environment it is promoted from.
    // Memoized so unrelated saved changes keep unsaved edits.
    let drafts = Memo::new(move |_| {
        let branch = current().map_or_else(
            || default_branch(context),
            |branch| {
                (
                    branch.branch().to_owned(),
                    branch.commit().unwrap_or_default().to_owned(),
                )
            },
        );
        (branch, promoted().unwrap_or_default())
    });
    let (initial_branch, initial_source) = drafts.get_untracked();
    let branch = RwSignal::new(initial_branch);
    // The environment to promote from; empty to build its own source.
    let promote_from = RwSignal::new(initial_source);
    let builds = move || promote_from.with(String::is_empty);
    let sources = Signal::derive(move || context.promotion_sources(&id.get_value()));
    let name = move || context.selected_name();
    let declared = move || context.source_declared(&name());
    let blocked = move || {
        context.change_blocked(Some(SOURCE_FORM)) || context.selected_deleting() || declared()
    };
    let sync_blocked =
        move || context.action_blocked() || context.selected_deleting() || declared();
    // The change saving would make, if any.
    let change = move || -> Option<Result<EnvironmentEdit, String>> {
        let id = id.get_value();
        if !builds() {
            let from = promote_from.get();
            return (Some(&from) != promoted().as_ref()).then_some(Ok(EnvironmentEdit::Source {
                id,
                promote_from: Some(from),
            }));
        }
        if repository() {
            return match tracked_branch(branch.get()) {
                Ok(tracked) if promoted().is_none() && Some(&tracked) == current().as_ref() => None,
                Ok(tracked) => Some(Ok(EnvironmentEdit::Branch {
                    id,
                    branch: tracked,
                })),
                Err(error) => Some(Err(error)),
            };
        }
        promoted().map(|_| {
            Ok(EnvironmentEdit::Source {
                id,
                promote_from: None,
            })
        })
    };
    // A change not yet saved blocks deploys and guards navigation. The form
    // follows the saved source while it still shows the previous one.
    context.guard(SOURCE_FORM.into(), move || change().is_some());
    Effect::new(move |previous: Option<((String, String), String)>| {
        let saved = drafts.get();
        let shown = untrack(|| (branch.get(), promote_from.get()));
        if previous.is_none_or(|previous| previous == shown) {
            branch.set(saved.0.clone());
            promote_from.set(saved.1.clone());
        }
        saved
    });
    let save = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        match change() {
            Some(Ok(change)) => context.change_environment(change, |_| {}),
            Some(Err(error)) => context.set_error(Some(error)),
            None => {}
        }
    };
    let sync = move || {
        current()?;
        let environment = context.selected_environment()?;
        let state = context
            .saved
            .with(|saved| environment.sync_state(saved.application.spec().manifest.as_ref()));
        Some((environment, state))
    };
    let toggle_sync = move |_| {
        if let Some((environment, _)) = sync() {
            let change = EnvironmentEdit::Sync {
                id: id.get_value(),
                enabled: !environment.sync,
            };
            context.change_environment(change, |_| {});
        }
    };
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>"Source"</h3>
                    <p>
                        {move || match (promoted(), current()) {
                            (Some(from), _) => {
                                view! {
                                    "Receives releases promoted from "
                                    <A href={context
                                        .environment_href(
                                            &from,
                                        )}>{context.environment_name(&from)}</A>
                                    ", and never builds or fetches."
                                }
                                    .into_any()
                            }
                            (None, Some(branch)) => {
                                format!(
                                    "Builds the manifest on branch {branch} of the application's repository.",
                                )
                                    .into_any()
                            }
                            (None, None) => "Builds the application's saved manifest.".into_any(),
                        }}
                    </p>
                </div>
            </header>
            {move || declared().then(|| declared_notice(&name()))}
            <form class="stack-sm" on:submit={save}>
                <fieldset class="stack-sm" disabled={blocked}>
                    <PromotionSource
                        value={promote_from}
                        sources={sources}
                        none="Builds its own source"
                        label="Deploys"
                        prefix="Releases promoted from "
                    />
                    <Show when={move || builds() && repository()}>
                        <div class="form-grid">{branch_fields(branch)}</div>
                    </Show>
                </fieldset>
                <p class="hint">
                    {move || {
                        match (builds(), repository()) {
                            (false, _) => {
                                "Promoted: never builds or fetches. Deploy it with Promote, which takes the release its source runs."
                            }
                            (true, true) => {
                                "Builds: each deployment fetches the manifest from this branch, or the pinned commit, and builds it."
                            }
                            (true, false) => {
                                "Builds: each deployment builds the application's saved manifest."
                            }
                        }
                    }}
                    " Saving redeploys nothing; the change applies from the next deployment or promotion."
                </p>
                <div class="form-actions">
                    <button
                        type="submit"
                        class="btn"
                        disabled={move || blocked() || change().is_none()}
                    >
                        "Save source"
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
                                <dd>{badge} <div class="muted">{reason}</div></dd>
                                <dt>"Last synced"</dt>
                                <dd>{last_synced(environment.synced)}</dd>
                            </dl>
                            <div class="form-actions">
                                <button
                                    type="button"
                                    class="btn"
                                    disabled={sync_blocked}
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
        </section>
    }
}

/// Whether the environment page's environment is granted the daemon's
/// telemetry, with a button granting or revoking it for holders of
/// `telemetry:grant`.
#[component]
fn EnvironmentTelemetry() -> impl IntoView {
    let context = editor();
    let id = StoredValue::new(context.environment_id());
    let allowed = can(Permission::Global(GlobalPermission::TelemetryGrant));
    let granted = move || {
        context
            .selected_environment()
            .is_some_and(|environment| environment.telemetry)
    };
    let toggle = move |_| {
        let granted = !granted();
        if granted
            && !confirm(&format!(
                "Grant environment {} the daemon's telemetry? Its receiving service will get the traces, metrics and logs of every application.",
                context.selected_name()
            ))
        {
            return;
        }
        let change = EnvironmentEdit::Telemetry {
            id: id.get_value(),
            granted,
        };
        context.change_environment(change, |_| {});
    };
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>"Telemetry"</h3>
                    <p>
                        "A granted environment's receiving service, the one that sets receives_telemetry, gets the daemon's traces, metrics and logs from every application over a socket mounted at /run/piqueld/telemetry. Revoking stops export at once; either way the socket is mounted or unmounted within seconds, which restarts the service."
                    </p>
                </div>
            </header>
            <dl class="kv">
                <dt>"Grant"</dt>
                <dd>
                    {move || {
                        if granted() {
                            badge(Tone::Ok, "granted")
                        } else {
                            badge(Tone::Neutral, "not granted")
                        }
                    }}
                </dd>
            </dl>
            <div class="form-actions">
                <button
                    type="button"
                    class="btn"
                    disabled={move || {
                        !allowed.get() || context.action_blocked() || context.selected_deleting()
                    }}
                    title={move || {
                        (!allowed.get()).then_some("Requires the telemetry:grant permission.")
                    }}
                    on:click={toggle}
                >
                    {move || if granted() { "Revoke telemetry" } else { "Grant telemetry" }}
                </button>
            </div>
        </section>
    }
}

impl EditorContext {
    /// The visibility ceiling `[spec.environments.<name>]` gives the shown
    /// environment: in the `saved` manifest its form saves to, or while
    /// managed in Git, read-only, in the manifest it last fetched.
    fn visibility(self, saved: &ApplicationManifest) -> Visibility {
        let name = self.selected_name();
        let read =
            |spec: &ApplicationSpec| spec.environments.get(&name).map(|config| config.visibility);
        if self.managed() {
            self.environment_manifest()
                .and_then(|manifest| read(manifest.spec()))
        } else {
            read(&saved.spec)
        }
        .unwrap_or(Visibility::Public)
    }
}

/// The visibility ceiling of the environment page's environment, saved in
/// `[spec.environments.<name>]`. Read-only while the application is managed
/// in Git.
#[component]
fn EnvironmentVisibility() -> impl IntoView {
    let context = editor();
    let draft = Draft::synced("environment-visibility", move |saved| {
        context.visibility(saved)
    });
    let promoted = move || {
        context
            .selected_environment()
            .is_some_and(|environment| environment.source.promoted_from().is_some())
    };
    let save = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        context.save(
            ApplicationEdit::EnvironmentVisibility {
                environment: context.selected_name(),
                visibility: draft.value.get_untracked(),
            },
            move |_| draft.discard(),
        );
    };
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>"Visibility"</h3>
                    <p>
                        "The strictest visibility this environment's routes get. Private keeps every route on the tailnet, whatever the route itself says; public leaves each route's own visibility. Save, then deploy to apply it."
                        {move || {
                            promoted()
                                .then_some(
                                    " A promoted environment uses the visibility recorded in the release it receives, so a saved change applies once its source records a new release and it is promoted.",
                                )
                        }}
                    </p>
                </div>
            </header>
            <form class="stack-sm" on:submit={save}>
                <fieldset
                    class="stack-sm"
                    disabled={move || context.blocked() || context.managed()}
                >
                    <label class="field">
                        <span>"Route visibility"</span>
                        <select
                            prop:value={move || draft.value.get().to_string()}
                            on:change={move |event| {
                                if let Ok(visibility) = event_target_value(&event).parse() {
                                    draft.value.set(visibility);
                                }
                            }}
                        >
                            <option value="public">
                                "Public: routes keep their own visibility"
                            </option>
                            <option value="private">"Private: every route is tailnet-only"</option>
                        </select>
                    </label>
                </fieldset>
                <div class="form-actions">
                    <button
                        type="submit"
                        class="btn"
                        disabled={move || {
                            context.blocked() || context.managed() || !draft.changed()
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
#[expect(clippy::too_many_lines, reason = "flat view! markup")]
pub(super) fn EnvironmentSettings() -> impl IntoView {
    let context = editor();
    let navigate = use_navigate();
    let id = StoredValue::new(context.environment_id());
    let current = move || context.selected_name();
    let draft = Draft::new(NAME_FORM.into(), current());
    Effect::new(move |_| draft.follow(current()));
    let name = draft.value;
    let deleting = move || context.selected_deleting();
    let rename = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        let value = name.get_untracked();
        if let Err(error) = EnvironmentName::parse(&value) {
            context.set_error(Some(error.to_string()));
            return;
        }
        context.change_environment(
            EnvironmentEdit::Rename {
                id: id.get_value(),
                name: value,
            },
            |_| {},
        );
    };
    let delete = move |_| {
        if deleting() {
            context.change_environment(EnvironmentEdit::RetryDeletion(id.get_value()), |_| {});
            return;
        }
        let message = format!(
            "Delete environment {}, its services, secrets and deployment history? The application and other environments remain. Docker volume data will be retained.",
            current()
        );
        if !confirm(&message) {
            return;
        }
        let navigate = navigate.clone();
        let href = format!("/dashboard/applications/{}?tab=environments", context.id());
        context.change_environment(EnvironmentEdit::Delete(id.get_value()), move |_| {
            navigate(&href, NavigateOptions::default());
        });
    };
    view! {
        <SourceSettings />
        <EnvironmentVisibility />
        <EnvironmentTelemetry />
        <section class="card">
            <header>
                <div>
                    <h3>"Environment"</h3>
                    <p>"Renaming changes only the name; nothing is redeployed."</p>
                </div>
            </header>
            <form class="stack-sm" on:submit={rename}>
                <fieldset
                    class="stack-sm"
                    disabled={move || {
                        context.change_blocked(Some(NAME_FORM)) || deleting()
                            || context.declared(&current())
                    }}
                >
                    {text_field("Environment name", name)}
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
                            context.change_blocked(Some(NAME_FORM)) || deleting() || !draft.changed()
                                || context.declared(&current())
                        }}
                        title={move || {
                            context
                                .declared(&current())
                                .then(|| {
                                    format!(
                                        "The manifest declares [spec.environments.{}]; rename that block instead.",
                                        current(),
                                    )
                                })
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
                    disabled={move || context.action_blocked() || context.declared(&current())}
                    title={move || {
                        context
                            .declared(&current())
                            .then(|| {
                                format!(
                                    "Remove [spec.environments.{}] from the manifest first.",
                                    current(),
                                )
                            })
                    }}
                    on:click={delete}
                >
                    {move || if deleting() { "Retry deletion" } else { "Delete environment" }}
                </button>
            </header>
        </section>
    }
}
