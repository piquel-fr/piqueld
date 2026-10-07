//! Environment lifecycle controls and the branch an environment follows;
//! configuration continues to belong to the application or its repository.
use super::super::environment_row;
use super::super::ui::{
    Icon, Modal, Tone, badge, empty, health_badge, icon, notice, operation_badge, text_input, when,
};
use super::{EditorContext, editor, mutation_client, transport_failure};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::NavigateOptions;
use leptos_router::components::A;
use leptos_router::hooks::use_navigate;
use piqueld_client::{
    Client, ClientError, CreateEnvironmentRequest, EnvironmentBranchRequest, EnvironmentName,
    EnvironmentRequest, EnvironmentView, TrackedBranch,
};

enum EnvironmentChange {
    Create {
        name: String,
        branch: Option<TrackedBranch>,
    },
    Rename {
        id: String,
        name: String,
    },
    Branch {
        id: String,
        branch: TrackedBranch,
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
            Self::Create { name, branch } => {
                let request = CreateEnvironmentRequest {
                    name: name.clone(),
                    branch: branch.as_ref().map(|branch| branch.branch().to_owned()),
                    commit: branch
                        .as_ref()
                        .and_then(|branch| branch.commit().map(str::to_owned)),
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
                        if !matches!(change, EnvironmentChange::RetryDeletion(_)) {
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
/// Each whole row opens the environment's page; its Deploy button deploys it.
/// Also creates new environments.
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
                                let deployments = format!("{href}?tab=deployments");
                                let navigate = navigate.clone();
                                let deleting = environment.delete_intent;
                                let name = environment.name.to_string();
                                let source = environment.source.to_string();
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
                                        <button
                                            type="button"
                                            class="btn btn-sm"
                                            aria-label={label}
                                            disabled={move || context.action_blocked() || deleting}
                                            on:click={move |_| {
                                                let navigate = navigate.clone();
                                                let deployments = deployments.clone();
                                                context
                                                    .deploy(
                                                        id.clone(),
                                                        move || navigate(&deployments, NavigateOptions::default()),
                                                    );
                                            }}
                                        >
                                            {icon(Icon::Rocket)}
                                            "Deploy"
                                        </button>
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

/// "New environment" button and dialog; opens the created environment. For a
/// repository-backed application it also chooses the branch to follow.
#[component]
fn NewEnvironment() -> impl IntoView {
    let context = editor();
    let opened = RwSignal::new(false);
    let name = RwSignal::new(String::new());
    let branch = RwSignal::new(default_branch(context));
    let navigate = use_navigate();
    let submit = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        let value = name.get_untracked();
        if let Err(error) = EnvironmentName::parse(&value) {
            context.set_error(Some(error.to_string()));
            return;
        }
        let tracked = if context.managed() {
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
        };
        context.change_environment(change, move |environment| {
            opened.set(false);
            name.set(String::new());
            branch.set(default_branch(context));
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
                branch.set(default_branch(context));
                opened.set(true);
            }}
        >
            {icon(Icon::Plus)}
            "New environment"
        </button>
        <Modal
            title="Create environment"
            opened={opened}
            busy={context.busy}
            on_close={Callback::new(move |()| name.set(String::new()))}
        >
            <form class="stack-sm" on:submit={submit}>
                <fieldset class="stack-sm" disabled={move || context.action_blocked()}>
                    <p class="hint">
                        {move || {
                            if context.managed() {
                                "The environment starts empty and deploys the manifest on its own branch of the application's repository, with its own secrets, volumes, and history."
                            } else {
                                "The environment starts empty and deploys the application's configuration with its own secrets, volumes, and history."
                            }
                        }}
                    </p>
                    {text_input("Environment name", name, String::clone, |value, input| *value = input)}
                    <Show when={move || context.managed()}>
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

/// The branch the environment page's environment follows, with a form to point
/// it at another branch or pin a commit. Environments deploying the saved
/// manifest only say so.
#[component]
fn EnvironmentBranch() -> impl IntoView {
    let context = editor();
    let id = StoredValue::new(context.environment_id());
    let current = move || {
        context
            .selected_environment()
            .and_then(|environment| environment.source.branch().cloned())
    };
    let draft = move || {
        current().map_or_else(Default::default, |branch| {
            (
                branch.branch().to_owned(),
                branch.commit().unwrap_or_default().to_owned(),
            )
        })
    };
    let branch = RwSignal::new(draft());
    let deleting = move || {
        context
            .selected_environment()
            .is_some_and(|environment| environment.delete_intent)
    };
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
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>"Source"</h3>
                    <p>
                        {move || {
                            if current().is_some() {
                                "Deploys the manifest on this branch of the application's repository. Changing it redeploys nothing; the next deployment fetches the new branch."
                            } else {
                                "Deploys the application's saved manifest."
                            }
                        }}
                    </p>
                </div>
            </header>
            <Show when={move || current().is_some()}>
                <form class="stack-sm" on:submit={save}>
                    <fieldset
                        class="form-grid"
                        disabled={move || context.action_blocked() || deleting()}
                    >
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
                                context.action_blocked() || deleting() || branch.get() == draft()
                            }}
                        >
                            "Change branch"
                        </button>
                    </div>
                </form>
            </Show>
        </section>
    }
}

/// Source, rename and deletion of the environment page's environment.
/// Deletion returns to the application's environments once accepted.
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
        <EnvironmentBranch />
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
