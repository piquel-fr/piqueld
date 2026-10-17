//! Write-only secret values, their access lists, and saved file references.
use super::super::ui::{
    Icon, Tone, add_button, badge, checkbox, empty, icon, member_checkbox, notice, remove_button,
    text_field, when,
};
use super::super::{ClientErrorExt, confirm};
use super::{Draft, EditorContext, Rows, editor};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use piqueld_client::{
    ApplicationView, Client, ClientError, EnvironmentAccess, EnvironmentId, MountedSecret,
    SecretAccess, SecretMetadata, SecretMount, StoredSecret, Template,
    edit::{ApplicationEdit, ServiceEdit},
};
use std::collections::BTreeSet;

/// Outcome messages shared by the secret views: an error with its diagnostic
/// link, or a success notice.
#[derive(Clone, Copy)]
struct Feedback {
    error: RwSignal<Option<String>>,
    diagnostic: RwSignal<Option<String>>,
    notice: RwSignal<String>,
}

impl Feedback {
    fn new() -> Self {
        Self {
            error: RwSignal::new(None),
            diagnostic: RwSignal::new(None),
            notice: RwSignal::new(String::new()),
        }
    }

    /// Shows `message` for the failed request `error`, with its diagnostic.
    fn fail(self, message: String, error: &ClientError) {
        self.notice.set(String::new());
        self.diagnostic.set(error.diagnostic_id());
        self.error.set(Some(message));
    }

    /// Shows `message` instead of any earlier outcome.
    fn succeed(self, message: &str) {
        self.clear();
        self.notice.set(message.into());
    }

    /// Hides the last outcome.
    fn clear(self) {
        self.diagnostic.set(None);
        self.error.set(None);
        self.notice.set(String::new());
    }

    /// Sends `request` while the editor is busy, replacing the last outcome,
    /// then runs `done` with its response or `failed` with its error.
    fn send<T: 'static>(
        self,
        context: EditorContext,
        request: impl Future<Output = Result<T, ClientError>> + 'static,
        done: impl FnOnce(T) + 'static,
        failed: impl FnOnce(ClientError) + 'static,
    ) {
        context.busy.set(true);
        self.clear();
        spawn_local(async move {
            match request.await {
                Ok(response) => done(response),
                Err(error) => failed(error),
            }
            context.busy.set(false);
        });
    }

    /// The error with its diagnostic link, or the success notice.
    fn view(self) -> impl IntoView {
        view! {
            {move || {
                self.error
                    .get()
                    .map(|e| {
                        notice(
                            Tone::Bad,
                            view! {
                                <span>{e}</span>
                                {self
                                    .diagnostic
                                    .get()
                                    .map(|id| {
                                        view! {
                                            <A href={format!(
                                                "/dashboard/errors/{id}",
                                            )}>"Diagnostic details"</A>
                                        }
                                    })}
                            },
                        )
                    })
            }}
            {move || {
                (!self.notice.get().is_empty()).then(|| notice(Tone::Ok, self.notice.get()))
            }}
        }
    }
}

/// Loads `load` into `items` once now and on each `reload`, marking `ready`.
fn loader<
    T: Send + Sync + 'static,
    F: std::future::Future<Output = Result<Vec<T>, ClientError>>,
>(
    items: RwSignal<Vec<T>>,
    ready: RwSignal<bool>,
    feedback: Feedback,
    load: impl Fn() -> F + Copy + Send + Sync + 'static,
) -> Callback<()> {
    let loading = RwSignal::new(false);
    let reload = Callback::new(move |()| {
        if loading.get_untracked() {
            return;
        }
        loading.set(true);
        ready.set(false);
        spawn_local(async move {
            match load().await {
                Ok(loaded) => {
                    items.set(loaded);
                    ready.set(true);
                }
                Err(e) => feedback.fail(e.user_message(), &e),
            }
            loading.set(false);
        });
    });
    reload.run(());
    reload
}

/// Badge of a secret's state: being deleted, its value discarded, or `held`.
fn status_badge(metadata: &SecretMetadata, held: &'static str) -> AnyView {
    if metadata.deleting {
        badge(Tone::Warn, "deletion pending")
    } else if metadata.unavailable {
        badge(Tone::Bad, "value discarded")
    } else {
        badge(Tone::Ok, held)
    }
}

/// The access list a secret form edits: every environment, or the checked
/// environment IDs, and whether previews may mount the secret.
#[derive(Clone, Copy)]
struct AccessForm {
    all: RwSignal<bool>,
    allowed: RwSignal<BTreeSet<EnvironmentId>>,
    previews: RwSignal<bool>,
}

impl AccessForm {
    fn new() -> Self {
        let form = Self {
            all: RwSignal::new(true),
            allowed: RwSignal::new(BTreeSet::new()),
            previews: RwSignal::new(false),
        };
        form.load(&SecretAccess::default());
        form
    }

    /// Shows `access` in the form.
    fn load(self, access: &SecretAccess) {
        match &access.environments {
            EnvironmentAccess::All => {
                self.all.set(true);
                self.allowed.set(BTreeSet::new());
            }
            EnvironmentAccess::Only(ids) => {
                self.all.set(false);
                self.allowed.set(ids.clone());
            }
        }
        self.previews.set(access.previews);
    }

    /// The access the form shows.
    fn access(self) -> SecretAccess {
        SecretAccess {
            environments: if self.all.get_untracked() {
                EnvironmentAccess::All
            } else {
                EnvironmentAccess::Only(self.allowed.get_untracked())
            },
            previews: self.previews.get_untracked(),
        }
    }

    /// Checkboxes for every environment, all of them, and previews.
    fn view(self) -> impl IntoView {
        let context = editor();
        view! {
            <div class="stack-sm">
                <span>"Who may mount it"</span>
                {checkbox("Every environment, including ones created later", self.all)}
                <div hidden={move || self.all.get()} class="stack-sm">
                    <For
                        each={move || context.saved.get().environments}
                        key={|environment| environment.id.clone()}
                        children={move |environment| {
                            view! {
                                <label class="checkbox">
                                    {member_checkbox(self.allowed, environment.id)}
                                    {environment.name.to_string()}
                                </label>
                            }
                        }}
                    />
                </div>
                {checkbox("Previews", self.previews)}
                <p class="hint">
                    "Previews do not exist yet; the setting is kept for them. Access applies to later deployments; running ones keep their versions."
                </p>
            </div>
        }
    }
}

/// The application's secret store: manually set values (never read back),
/// their versions and access lists. Writes are guarded by generation and
/// clear the value field as soon as they are submitted; after a failure,
/// actions stay disabled until the list is refreshed. On an environment's
/// page, given the secrets its manifest `mounts`, it lists only the secrets
/// that environment may mount or mounts, and new ones default to it alone;
/// `changed` runs after every write.
#[component]
fn StoredSecrets(
    #[prop(optional, into)] mounts: Option<Signal<BTreeSet<String>>>,
    #[prop(optional)] changed: Option<Callback<()>>,
) -> impl IntoView {
    let context = editor();
    // The environment page's environment, when scoped to it.
    let scope = move || mounts.and_then(|_| context.selected_environment());
    let secrets = RwSignal::new(Vec::<StoredSecret>::new());
    let ready = RwSignal::new(false);
    let feedback = Feedback::new();
    let name = RwSignal::new(String::new());
    // A typed value is an unsaved edit.
    let value = Draft::new("secret-value".into(), String::new()).value;
    let form = AccessForm::new();
    let id = StoredValue::new(context.id());
    let reload = loader(secrets, ready, feedback, move || {
        let id = id.get_value();
        async move { Client::browser().stored_secrets(&id).await }
    });
    let existing = move |name: &str| {
        secrets.with_untracked(|items| {
            items
                .iter()
                .find(|s| s.metadata.name == name)
                .map(|s| (s.metadata.generation, s.access.clone()))
        })
    };
    // New secrets default to every environment, or on an environment's
    // page to that environment, or on a preview's to previews.
    let default_access = move || match scope() {
        Some(environment) if environment.preview().is_some() => SecretAccess {
            environments: EnvironmentAccess::Only(BTreeSet::new()),
            previews: true,
        },
        Some(environment) => SecretAccess {
            environments: EnvironmentAccess::Only(BTreeSet::from([environment.id])),
            previews: false,
        },
        None => SecretAccess::default(),
    };
    form.load(&default_access());
    // Naming a stored secret, or refreshing the list, loads its access;
    // naming a new one after it starts again from the default.
    Effect::new(move |editing: Option<bool>| {
        let name = name.get();
        let access = secrets.with(|items| {
            items
                .iter()
                .find(|s| s.metadata.name == name)
                .map(|s| s.access.clone())
        });
        match &access {
            Some(access) => form.load(access),
            None if editing == Some(true) => form.load(&default_access()),
            None => {}
        }
        access.is_some()
    });
    let notify = move || {
        if let Some(changed) = changed {
            changed.run(());
        }
    };
    let saved = move |secret: StoredSecret, message: &str| {
        secrets.update(|items| {
            items.retain(|s| s.metadata.name != secret.metadata.name);
            items.push(secret);
            items.sort_by(|a, b| a.metadata.name.cmp(&b.metadata.name));
        });
        feedback.succeed(message);
        notify();
    };
    let failed = move |e: ClientError| {
        ready.set(false);
        feedback.fail(
            format!(
                "{} Refresh before another secret change. Any submitted value has been cleared.",
                e.user_message()
            ),
            &e,
        );
    };
    // Whether a change may start now.
    let idle = move || !context.blocked() && ready.get_untracked();
    let write = move |_| {
        if !idle() {
            return;
        }
        let name = name.get_untracked();
        let (generation, current) = existing(&name).unzip();
        let generation = generation.unwrap_or(0);
        if generation > 0
            && !confirm(&format!(
                "Replace {name}? Running deployments keep their current version. A later Deploy will use the replacement."
            ))
        {
            return;
        }
        let bytes = value.get_untracked().into_bytes();
        value.set(String::new());
        // Replacing a value sends access only when the form changed it, so the
        // server keeps a list changed elsewhere.
        let access = Some(form.access()).filter(|access| current.as_ref() != Some(access));
        let id = id.get_value();
        feedback.send(
            context,
            async move {
                Client::browser()
                    .put_stored_secret(&id, &name, generation, bytes, access.as_ref())
                    .await
            },
            move |secret| saved(secret, "Secret saved. Deploy to use its new version."),
            failed,
        );
    };
    let change_access = move |_| {
        if !idle() {
            return;
        }
        let (id, name, access) = (id.get_value(), name.get_untracked(), form.access());
        feedback.send(
            context,
            async move {
                Client::browser()
                    .set_secret_access(&id, &name, &access)
                    .await
            },
            move |secret| saved(secret, "Access saved. It applies to later deployments."),
            failed,
        );
    };
    let remove = Callback::new(move |secret: SecretMetadata| {
        if !idle() {
            return;
        }
        if !confirm(&format!(
            "Delete {} from every environment, with its retained versions? Secrets still in use cannot be deleted.",
            secret.name
        )) {
            return;
        }
        let id = id.get_value();
        let name = secret.name.clone();
        feedback.send(
            context,
            async move {
                Client::browser()
                    .delete_stored_secret(&id, &secret.name, secret.generation)
                    .await
            },
            move |()| {
                secrets.update(|items| items.retain(|s| s.metadata.name != name));
                feedback.succeed("Secret deleted.");
                notify();
            },
            failed,
        );
    });
    // Every secret, or on an environment's page the ones it may mount or mounts.
    let shown = move || {
        let scope = scope();
        let mounts = mounts.map(|mounts| mounts.get()).unwrap_or_default();
        secrets
            .get()
            .into_iter()
            .filter(|secret| {
                scope.as_ref().is_none_or(|environment| {
                    secret.access.allows(environment) || mounts.contains(&secret.metadata.name)
                })
            })
            .collect::<Vec<_>>()
    };
    let rows = move || {
        let environments = context.saved.with(|saved| saved.environments.clone());
        shown()
            .into_iter()
            .map(|secret| {
                let selected = secret.metadata.name.clone();
                let metadata = secret.metadata.clone();
                let deleting = metadata.deleting;
                view! {
                    <tr>
                        <td>
                            <strong>{metadata.name.clone()}</strong>
                        </td>
                        <td class="num">{metadata.generation}</td>
                        <td class="muted">{when(metadata.updated_at_ms)}</td>
                        <td>{secret.access.describe(&environments)}</td>
                        <td>{status_badge(&metadata, "stored")}</td>
                        <td class="actions">
                            <span class="btn-group" style="justify-content:flex-end">
                                <button
                                    type="button"
                                    class="btn btn-sm"
                                    disabled={move || context.blocked() || !ready.get() || deleting}
                                    on:click={move |_| name.set(selected.clone())}
                                >
                                    "Edit"
                                </button>
                                <button
                                    type="button"
                                    class="btn btn-ghost btn-sm"
                                    disabled={move || context.blocked() || !ready.get()}
                                    on:click={move |_| remove.run(metadata.clone())}
                                >
                                    "Delete"
                                </button>
                            </span>
                        </td>
                    </tr>
                }
            })
            .collect_view()
    };
    let stored = move || secrets.with(|items| items.iter().any(|s| s.metadata.name == name.get()));
    let deleting = move || {
        secrets.with(|items| {
            items
                .iter()
                .any(|s| s.metadata.name == name.get() && s.metadata.deleting)
        })
    };
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>"Secret store"</h3>
                    <p>
                        {move || {
                            if scope().is_some() {
                                "Manually set values this environment may mount or mounts, from the application's store, write-only. New ones may be mounted by this environment only (by previews, on a preview), unless you choose otherwise; deploying or promoting it while it mounts a secret it may not use fails before rollout."
                            } else {
                                "Manually set values, shared by this application's environments and write-only. Each lists the environments that may mount it; deploying an environment that mounts a secret it may not use fails before rollout."
                            }
                        }}
                    </p>
                </div>
                <button
                    type="button"
                    class="btn btn-sm"
                    disabled={move || context.blocked()}
                    on:click={move |_| reload.run(())}
                >
                    {icon(Icon::Refresh)}
                    "Refresh"
                </button>
            </header>
            <div class="stack-sm">
                {feedback.view()}
                {move || {
                    secrets
                        .with(|items| items.iter().any(|s| s.metadata.unavailable))
                        .then(|| {
                            notice(
                                Tone::Warn,
                                "Some values were discarded by secret key recovery. Supply a replacement value for each, then deploy.",
                            )
                        })
                }}
                <div class="table-wrap">
                    {move || {
                        if shown().is_empty() {
                            if ready.get() {
                                empty(
                                    if scope().is_some() {
                                        "No stored secret this environment may mount."
                                    } else {
                                        "No secrets stored for this application."
                                    },
                                )
                            } else {
                                empty("Loading secrets…")
                            }
                        } else {
                            view! {
                                <table class="table">
                                    <thead>
                                        <tr>
                                            <th>"Name"</th>
                                            <th class="num">"Version"</th>
                                            <th>"Updated"</th>
                                            <th>"Mountable by"</th>
                                            <th>"Status"</th>
                                            <th></th>
                                        </tr>
                                    </thead>
                                    <tbody>{rows()}</tbody>
                                </table>
                            }
                                .into_any()
                        }
                    }}
                </div>
                <fieldset class="stack-sm" disabled={move || context.blocked() || !ready.get()}>
                    <div class="section-header">
                        <h4>"Save a secret"</h4>
                    </div>
                    {text_field("Secret name", name)}
                    <label class="field">
                        <span>"Value"</span>
                        <textarea
                            autocomplete="off"
                            spellcheck="false"
                            rows="3"
                            prop:value={move || value.get()}
                            on:input={move |e| value.set(event_target_value(&e))}
                        ></textarea>
                    </label>
                    <p class="hint">
                        "The value is cleared when submitted. Use the CLI for binary secret files."
                    </p>
                    {form.view()}
                    <div class="form-actions">
                        <button
                            type="button"
                            class="btn"
                            disabled={move || !stored()}
                            on:click={change_access}
                        >
                            "Save access only"
                        </button>
                        <button
                            type="button"
                            class="btn btn-primary"
                            disabled={move || {
                                name.get().is_empty() || value.get().is_empty() || deleting()
                            }}
                            on:click={write}
                        >
                            "Save secret"
                        </button>
                    </div>
                </fieldset>
            </div>
        </section>
    }
}

/// What the environment's Secrets tab can do to a generated secret.
#[derive(Clone, Copy)]
enum GeneratedAction {
    /// Generate a new version for the next deployment.
    Regenerate,
    /// Delete the value, so a later deployment generates a new one.
    Delete,
}

impl GeneratedAction {
    fn label(self) -> &'static str {
        match self {
            Self::Regenerate => "Regenerate",
            Self::Delete => "Delete",
        }
    }

    fn confirmation(self, name: &str) -> String {
        match self {
            Self::Regenerate => format!(
                "Generate a new value for {name}? Running deployments keep their current version. A later Deploy will use the new one."
            ),
            Self::Delete => format!(
                "Delete the generated value of {name} and its retained versions? A later deployment generates a new value. Secrets still in use cannot be deleted."
            ),
        }
    }

    fn success(self) -> &'static str {
        match self {
            Self::Regenerate => "New value generated. Deploy to use it.",
            Self::Delete => "Secret deleted.",
        }
    }

    /// Applies the action to `secret` of `environment`, returning the
    /// regenerated secret.
    async fn send(
        self,
        environment: &str,
        secret: &SecretMetadata,
    ) -> Result<Option<SecretMetadata>, ClientError> {
        let client = Client::browser();
        match self {
            Self::Regenerate => client
                .regenerate_secret(environment, &secret.name, secret.generation)
                .await
                .map(Some),
            Self::Delete => client
                .delete_secret(environment, &secret.name, secret.generation)
                .await
                .map(|()| None),
        }
    }
}

/// An environment's Secrets tab: where each secret the manifest it deploys
/// mounts comes from, and its generated values (never read back), which can
/// be regenerated for the next deployment, or deleted so a later deployment
/// generates new ones.
#[component]
#[expect(clippy::too_many_lines, reason = "flat view! markup")]
pub(super) fn EnvironmentSecrets() -> impl IntoView {
    let context = editor();
    let generated = RwSignal::new(Vec::<SecretMetadata>::new());
    let stored = RwSignal::new(Vec::<StoredSecret>::new());
    let ready = RwSignal::new(false);
    let stored_ready = RwSignal::new(false);
    let feedback = Feedback::new();
    let id = StoredValue::new(context.environment_id());
    let application = StoredValue::new(context.id());
    let reload_generated = loader(generated, ready, feedback, move || {
        let id = id.get_value();
        async move { Client::browser().secrets(&id).await }
    });
    let reload_stored = loader(stored, stored_ready, feedback, move || {
        let id = application.get_value();
        async move { Client::browser().stored_secrets(&id).await }
    });
    let act = Callback::new(move |(action, secret): (GeneratedAction, SecretMetadata)| {
        if context.blocked() || !ready.get_untracked() {
            return;
        }
        if !confirm(&action.confirmation(&secret.name)) {
            return;
        }
        let id = id.get_value();
        let name = secret.name.clone();
        feedback.send(
            context,
            async move { action.send(&id, &secret).await },
            move |replacement| {
                generated.update(|items| {
                    items.retain(|s| s.name != name);
                    items.extend(replacement);
                    items.sort_by(|a, b| a.name.cmp(&b.name));
                });
                feedback.succeed(action.success());
            },
            move |e| {
                ready.set(false);
                feedback.fail(
                    format!(
                        "{} Refresh, then retry if cleanup is pending.",
                        e.user_message()
                    ),
                    &e,
                );
            },
        );
    });
    let mounted = move || {
        let environment = context.selected_environment()?;
        let template = context.environment_manifest()?;
        Some(stored.with(|stored| MountedSecret::list(&template, &environment, stored)))
    };
    let mounted_names = Signal::derive(move || {
        mounted()
            .unwrap_or_default()
            .into_iter()
            .map(|(name, _)| name)
            .collect::<BTreeSet<_>>()
    });
    let mounted_rows = move || {
        mounted()
            .unwrap_or_default()
            .into_iter()
            .map(|(name, source)| {
                let tone = match source {
                    MountedSecret::Generated | MountedSecret::Stored { .. } => Tone::Ok,
                    MountedSecret::Missing => Tone::Warn,
                    MountedSecret::Denied | MountedSecret::Unavailable => Tone::Bad,
                };
                view! {
                    <tr>
                        <td>
                            <strong>{name}</strong>
                        </td>
                        <td>{badge(tone, source.to_string())}</td>
                    </tr>
                }
            })
            .collect_view()
    };
    let generated_rows = move || {
        generated
            .get()
            .into_iter()
            .map(|secret| {
                view! {
                    <tr>
                        <td>
                            <strong>{secret.name.clone()}</strong>
                        </td>
                        <td class="num">{secret.generation}</td>
                        <td class="muted">{when(secret.updated_at_ms)}</td>
                        <td>{status_badge(&secret, "generated")}</td>
                        <td class="actions">
                            {[GeneratedAction::Regenerate, GeneratedAction::Delete]
                                .map(|action| {
                                    let secret = secret.clone();
                                    view! {
                                        <button
                                            type="button"
                                            class="btn btn-ghost btn-sm"
                                            disabled={move || context.blocked() || !ready.get()}
                                            on:click={move |_| act.run((action, secret.clone()))}
                                        >
                                            {action.label()}
                                        </button>
                                    }
                                })}
                        </td>
                    </tr>
                }
            })
            .collect_view()
    };
    view! {
        <div class="stack">
            <section class="card">
                <header>
                    <div>
                        <h3>"Mounted secrets"</h3>
                        <p>
                            "Where each secret this environment's manifest mounts comes from. Set stored values below."
                        </p>
                    </div>
                    <button
                        type="button"
                        class="btn btn-sm"
                        disabled={move || context.blocked()}
                        on:click={move |_| {
                            reload_stored.run(());
                            reload_generated.run(());
                        }}
                    >
                        {icon(Icon::Refresh)}
                        "Refresh"
                    </button>
                </header>
                <div class="stack-sm">
                    {feedback.view()}
                    <div class="table-wrap">
                        {move || match mounted() {
                            None => {
                                empty(
                                    if context
                                        .selected_environment()
                                        .is_some_and(|environment| {
                                            environment.source.promoted_from().is_some()
                                        })
                                    {
                                        "Its source has no release to promote yet."
                                    } else {
                                        "Deploy this environment to fetch the secrets its branch mounts."
                                    },
                                )
                            }
                            Some(rows) if rows.is_empty() => {
                                empty("No secrets are mounted in this environment.")
                            }
                            Some(_) => {
                                view! {
                                    <table class="table">
                                        <thead>
                                            <tr>
                                                <th>"Name"</th>
                                                <th>"Value"</th>
                                            </tr>
                                        </thead>
                                        <tbody>{mounted_rows()}</tbody>
                                    </table>
                                }
                                    .into_any()
                            }
                        }}
                    </div>
                </div>
            </section>
            <StoredSecrets mounts={mounted_names} changed={reload_stored} />
            <section class="card">
                <header>
                    <div>
                        <h3>"Generated secrets"</h3>
                        <p>
                            "Values piqueld generated for this environment, never shared with another one. Running deployments keep their versions."
                        </p>
                    </div>
                </header>
                <div class="table-wrap">
                    {move || {
                        if generated.with(Vec::is_empty) {
                            if ready.get() {
                                empty("No secrets generated for this environment.")
                            } else {
                                empty("Loading secrets…")
                            }
                        } else {
                            view! {
                                <table class="table">
                                    <thead>
                                        <tr>
                                            <th>"Name"</th>
                                            <th class="num">"Version"</th>
                                            <th>"Updated"</th>
                                            <th>"Status"</th>
                                            <th></th>
                                        </tr>
                                    </thead>
                                    <tbody>{generated_rows()}</tbody>
                                </table>
                            }
                                .into_any()
                        }
                    }}
                </div>
            </section>
        </div>
    }
}

/// The application's Secrets tab: its secret store, then every service's
/// secret file references.
#[component]
pub(super) fn SecretFileSettings() -> impl IntoView {
    let context = editor();
    view! {
        <div class="stack">
            <StoredSecrets />
            <For
                each={move || {
                    context
                        .saved
                        .get()
                        .application
                        .spec()
                        .services
                        .iter()
                        .map(|s| s.name.clone())
                        .collect::<Vec<_>>()
                }}
                key={|name| name.clone()}
                children={move |name| view! { <SecretFiles service_name={name} /> }}
            />
        </div>
    }
}

/// The secret file references `service` saves in `saved`.
fn secret_files(saved: &ApplicationView, service: &str) -> Option<Vec<SecretMount>> {
    saved
        .application
        .spec()
        .services
        .iter()
        .find(|s| s.name.as_str() == service)
        .map(|s| s.secrets.clone())
}

/// Editor for one service's secret file references (secret name and container
/// path), saved as part of the application configuration.
#[component]
fn SecretFiles(service_name: String) -> impl IntoView {
    let context = editor();
    let draft = Draft::new(
        format!("secret-files:{service_name}"),
        context
            .saved
            .with_untracked(|saved| secret_files(saved, &service_name))
            .unwrap_or_default(),
    );
    let mounts = draft.value;
    let service = StoredValue::new(service_name.clone());
    let save = move || {
        context.save(
            ApplicationEdit::Service {
                name: service.get_value(),
                edit: ServiceEdit::Secrets(mounts.get_untracked()),
            },
            Callback::new(move |saved: ApplicationView| {
                if let Some(secrets) = secret_files(&saved, &service.get_value()) {
                    draft.reset(secrets);
                }
            }),
        );
    };
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>{service_name} " · secret files"</h3>
                    <p>
                        "Mount secrets as files under /run/secrets. A name declared under spec.secrets is generated per environment; any other comes from the secret store above, and may reference variables, e.g. ${{ vars.stripe_key }}, so each environment mounts its own. This saves references only; deploy to mount the referenced versions."
                    </p>
                </div>
            </header>
            <div class="form-list">
                <For
                    each={move || mounts.indices()}
                    key={|i| *i}
                    children={move |index| {
                        view! {
                            <div class="form-row">
                                <label class="field">
                                    <span>"Secret name"</span>
                                    <input
                                        prop:value={move || {
                                            mounts.field(index, |m| m.name.to_string())
                                        }}
                                        on:input={move |e| {
                                            mounts
                                                .edit(index, |m| m.name = event_target_value(&e).into());
                                        }}
                                    />
                                </label>
                                <label class="field">
                                    <span>"Container path"</span>
                                    <input
                                        prop:value={move || {
                                            mounts.field(index, |m| m.target.clone())
                                        }}
                                        on:input={move |e| {
                                            mounts.edit(index, |m| m.target = event_target_value(&e));
                                        }}
                                    />
                                </label>
                                {remove_button(move || mounts.remove(index))}
                            </div>
                        }
                    }}
                />
            </div>
            {add_button(
                "Add secret file",
                || false,
                move || {
                    mounts
                        .update(|items| {
                            items
                                .push(SecretMount {
                                    name: Template::default(),
                                    target: "/run/secrets/".into(),
                                });
                        });
                },
            )}
            {draft.actions(save, || false)}
        </section>
    }
}
