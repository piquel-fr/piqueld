//! Environment lifecycle controls; configuration continues to belong to the application.
use super::super::ui::{Icon, Modal, Tone, icon, notice, text_input};
use super::{EditorContext, editor, mutation_client, transport_failure};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::NavigateOptions;
use leptos_router::hooks::query_signal_with_options;
use piqueld_client::{Client, ClientError, EnvironmentName, EnvironmentRequest, EnvironmentView};

enum EnvironmentChange {
    Create(String),
    Rename { id: String, name: String },
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
        match self {
            Self::Create(name) | Self::Rename { name, .. } => {
                let request = EnvironmentRequest {
                    name: name.clone(),
                    expected_generation: Some(generation),
                };
                match self {
                    Self::Rename { id, .. } => client.rename_environment(id, &request, false).await,
                    _ => {
                        client
                            .create_environment(application, &request, false)
                            .await
                    }
                }
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
        done: Callback<Option<EnvironmentView>>,
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
                    done.run(environment);
                    self.dashboard.with_value(|d| d.refresh.run(()));
                }
                Err(error) => self.failure(&error),
            }
            self.busy.set(false);
        });
    }
}

/// One manager works for applications with zero, one or several environments.
#[component]
pub(super) fn EnvironmentManager() -> impl IntoView {
    let context = editor();
    let opened = RwSignal::new(false);
    let editing = RwSignal::new(None::<String>);
    let name = RwSignal::new(String::new());
    let (_, select) = query_signal_with_options::<String>(
        "environment",
        NavigateOptions {
            scroll: false,
            ..Default::default()
        },
    );
    let submit = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        let value = name.get_untracked();
        if let Err(error) = EnvironmentName::parse(&value) {
            context.set_error(Some(error.to_string()));
            return;
        }
        let change = editing.get_untracked().map_or_else(
            || EnvironmentChange::Create(value.clone()),
            |id| EnvironmentChange::Rename {
                id,
                name: value.clone(),
            },
        );
        context.change_environment(
            change,
            Callback::new(move |environment: Option<EnvironmentView>| {
                if let Some(environment) = environment {
                    select.set(Some(environment.id.to_string()));
                }
                editing.set(None);
                name.set(String::new());
                opened.set(false);
            }),
        );
    };
    view! {
        <button
            type="button"
            class="btn btn-ghost btn-icon"
            aria-label="Manage environments"
            title="Manage environments"
            disabled={move || context.action_blocked()}
            on:click={move |_| opened.set(true)}
        >
            {icon(Icon::Settings)}
        </button>
        <Modal title="Manage environments" opened={opened} busy={context.busy} on_close={Callback::new(move |()| {
            editing.set(None);
            name.set(String::new());
        })}>
            <div class="stack-sm">
                <p class="hint">"Every environment deploys the shared application configuration with its own secrets, volumes and history."</p>
                <For each={move || context.saved.get().environments} key={|env| env.id.clone()} children={move |env| {
                    let id = env.id.to_string();
                    let rename_id = id.clone();
                    let environment = Signal::derive(move || context.saved.with(|saved| saved.environments.iter().find(|current| current.id == env.id).cloned().unwrap_or_else(|| env.clone())));
                    let deleting = move || environment.get().delete_intent;
                    view! {
                        <div class="section-header">
                            <span>{move || environment.get().name.to_string()}{move || deleting().then_some(" (deleting)")}</span>
                            <div class="btn-group">
                                <button type="button" class="btn btn-sm" disabled={move || context.action_blocked() || deleting()} on:click={move |_| {
                                    editing.set(Some(rename_id.clone()));
                                    name.set(environment.get_untracked().name.to_string());
                                }}>"Rename"</button>
                                <button type="button" class="btn btn-sm btn-danger" disabled={move || context.action_blocked()} on:click={move |_| {
                                    if deleting() {
                                        context.change_environment(EnvironmentChange::RetryDeletion(id.clone()), Callback::new(|_| {}));
                                        return;
                                    }
                                    let environment_name = environment.get_untracked().name;
                                    if window().confirm_with_message(&format!("Delete environment {environment_name}, its services, secrets and deployment history? The application and other environments remain. Docker volume data will be retained.")).unwrap_or(false) {
                                        let deleted = id.clone();
                                        context.change_environment(EnvironmentChange::Delete(id.clone()), Callback::new(move |_| {
                                            if editing.get_untracked().as_ref() == Some(&deleted) {
                                                editing.set(None);
                                                name.set(String::new());
                                            }
                                        }));
                                    }
                                }}>{move || if deleting() { "Retry deletion" } else { "Delete" }}</button>
                            </div>
                        </div>
                    }
                }} />
                <form class="stack-sm" on:submit={submit}>
                    <fieldset disabled={move || context.action_blocked()}>
                        {text_input("Environment name", name, String::clone, |value, input| *value = input)}
                    </fieldset>
                    <div class="form-actions">
                        <button type="submit" class="btn btn-primary" disabled={move || context.action_blocked()}>
                            {move || if editing.get().is_some() { "Rename environment" } else { "Create environment" }}
                        </button>
                        <Show when={move || editing.get().is_some()}>
                            <button type="button" class="btn" disabled={move || context.blocked()} on:click={move |_| {
                                editing.set(None);
                                name.set(String::new());
                            }}>"Cancel rename"</button>
                        </Show>
                    </div>
                </form>
                {move || context.error.get().map(|error| notice(Tone::Bad, error))}
            </div>
        </Modal>
    }
}
