//! Write-only secret values, their access lists, and saved file references.
use super::super::ui::{Icon, Tone, badge, empty, icon, notice, remove_button, text_input, when};
use super::{client_error_message, diagnostic_id, dirty_group, editor, save_actions};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use piqueld_client::{
    ApplicationView, Client, ClientError, EnvironmentAccess, EnvironmentId, MountedSecret,
    SecretAccess, SecretMetadata, StoredSecret,
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
    fn fail(self, message: String, error: &ClientError) {
        self.notice.set(String::new());
        self.diagnostic.set(diagnostic_id(error));
        self.error.set(Some(message));
    }
    fn succeed(self, message: &str) {
        self.clear();
        self.notice.set(message.into());
    }
    fn clear(self) {
        self.diagnostic.set(None);
        self.error.set(None);
        self.notice.set(String::new());
    }
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
                                            <A href={format!("/dashboard/errors/{id}")}>
                                                "Diagnostic details"
                                            </A>
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
                Err(e) => feedback.fail(client_error_message(&e), &e),
            }
            loading.set(false);
        });
    });
    reload.run(());
    reload
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
    fn view(self) -> impl IntoView {
        let context = editor();
        let checkbox = move |label: String, checked: Signal<bool>, set: Callback<bool>| {
            view! {
                <label class="checkbox">
                    <input
                        type="checkbox"
                        prop:checked={move || checked.get()}
                        on:change={move |e| set.run(event_target_checked(&e))}
                    />
                    {label}
                </label>
            }
        };
        view! {
            <div class="stack-sm">
                <span>"Who may mount it"</span>
                {checkbox(
                    "Every environment, including ones created later".into(),
                    self.all.into(),
                    Callback::new(move |checked| self.all.set(checked)),
                )}
                <div hidden={move || self.all.get()} class="stack-sm">
                    <For
                        each={move || context.saved.get().environments}
                        key={|environment| environment.id.clone()}
                        children={move |environment| {
                            let id = environment.id.clone();
                            let toggled = id.clone();
                            checkbox(
                                environment.name.to_string(),
                                Signal::derive(move || self.allowed.with(|ids| ids.contains(&id))),
                                Callback::new(move |checked| {
                                    self.allowed
                                        .update(|ids| {
                                            if checked {
                                                ids.insert(toggled.clone());
                                            } else {
                                                ids.remove(&toggled);
                                            }
                                        });
                                }),
                            )
                        }}
                    />
                </div>
                {checkbox(
                    "Previews".into(),
                    self.previews.into(),
                    Callback::new(move |checked| self.previews.set(checked)),
                )}
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
    let value = RwSignal::new(String::new());
    let empty_value = RwSignal::new(String::new());
    dirty_group("secret-value".into(), value, empty_value);
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
    // Naming a stored secret, or refreshing the list, loads its access.
    Effect::new(move |_| {
        let name = name.get();
        if let Some(access) = secrets.with(|items| {
            items
                .iter()
                .find(|s| s.metadata.name == name)
                .map(|s| s.access.clone())
        }) {
            form.load(&access);
        }
    });
    if let Some(environment) = mounts.and_then(|_| context.selected_environment()) {
        form.load(&SecretAccess {
            environments: EnvironmentAccess::Only(BTreeSet::from([environment.id])),
            previews: false,
        });
    }
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
                client_error_message(&e)
            ),
            &e,
        );
    };
    let write = move |_| {
        if context.blocked() || !ready.get_untracked() {
            return;
        }
        let name = name.get_untracked();
        let (generation, current) = existing(&name).unzip();
        let generation = generation.unwrap_or(0);
        if generation > 0 && !window().confirm_with_message(&format!("Replace {name}? Running deployments keep their current version. A later Deploy will use the replacement.")).unwrap_or(false) {
            return;
        }
        let bytes = value.get_untracked().into_bytes();
        value.set(String::new());
        // Replacing a value sends access only when the form changed it, so the
        // server keeps a list changed elsewhere.
        let access = Some(form.access()).filter(|access| current.as_ref() != Some(access));
        let id = id.get_value();
        context.busy.set(true);
        feedback.clear();
        spawn_local(async move {
            match Client::browser()
                .put_stored_secret(&id, &name, generation, bytes, access.as_ref())
                .await
            {
                Ok(secret) => saved(secret, "Secret saved. Deploy to use its new version."),
                Err(e) => failed(e),
            }
            context.busy.set(false);
        });
    };
    let change_access = move |_| {
        if context.blocked() || !ready.get_untracked() {
            return;
        }
        let (id, name, access) = (id.get_value(), name.get_untracked(), form.access());
        context.busy.set(true);
        feedback.clear();
        spawn_local(async move {
            match Client::browser()
                .set_secret_access(&id, &name, &access)
                .await
            {
                Ok(secret) => saved(secret, "Access saved. It applies to later deployments."),
                Err(e) => failed(e),
            }
            context.busy.set(false);
        });
    };
    let remove = Callback::new(move |secret: SecretMetadata| {
        if context.blocked() || !ready.get_untracked() {
            return;
        }
        if !window()
            .confirm_with_message(&format!(
                "Delete {} from every environment, with its retained versions? Secrets still in use cannot be deleted.",
                secret.name
            ))
            .unwrap_or(false)
        {
            return;
        }
        let id = id.get_value();
        context.busy.set(true);
        feedback.clear();
        spawn_local(async move {
            match Client::browser()
                .delete_stored_secret(&id, &secret.name, secret.generation)
                .await
            {
                Ok(()) => {
                    secrets.update(|items| items.retain(|s| s.metadata.name != secret.name));
                    feedback.succeed("Secret deleted.");
                    notify();
                }
                Err(e) => failed(e),
            }
            context.busy.set(false);
        });
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
                let selected = secret.clone();
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
                        <td>
                            {if deleting {
                                badge(Tone::Warn, "deletion pending")
                            } else if metadata.unavailable {
                                badge(Tone::Bad, "value discarded")
                            } else {
                                badge(Tone::Ok, "stored")
                            }}
                        </td>
                        <td class="actions">
                            <span class="btn-group" style="justify-content:flex-end">
                                <button
                                    type="button"
                                    class="btn btn-sm"
                                    disabled={move || context.blocked() || !ready.get() || deleting}
                                    on:click={move |_| name.set(selected.metadata.name.clone())}
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
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>"Secret store"</h3>
                    <p>
                        {move || {
                            if scope().is_some() {
                                "Manually set values this environment may mount or mounts, from the application's store, write-only. New ones may be mounted by this environment only, unless you choose otherwise; deploying or promoting it while it mounts a secret it may not use fails before rollout."
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
                                empty(if scope().is_some() {
                                    "No stored secret this environment may mount."
                                } else {
                                    "No secrets stored for this application."
                                })
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
                    {text_input("Secret name", name, String::clone, |v, s| *v = s)}
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
                                name.get().is_empty() || value.get().is_empty()
                                    || secrets
                                        .with(|items| {
                                            items
                                                .iter()
                                                .any(|s| s.metadata.name == name.get() && s.metadata.deleting)
                                        })
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
}

/// An environment's Secrets tab: where each secret the manifest it deploys
/// mounts comes from, and its generated values (never read back), which can
/// be regenerated for the next deployment, or deleted so a later deployment
/// generates new ones.
#[component]
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
        if !window()
            .confirm_with_message(&action.confirmation(&secret.name))
            .unwrap_or(false)
        {
            return;
        }
        let id = id.get_value();
        context.busy.set(true);
        feedback.clear();
        spawn_local(async move {
            let client = Client::browser();
            let (name, generation) = (&secret.name, secret.generation);
            let result = match action {
                GeneratedAction::Regenerate => client
                    .regenerate_secret(&id, name, generation)
                    .await
                    .map(Some),
                GeneratedAction::Delete => client
                    .delete_secret(&id, name, generation)
                    .await
                    .map(|()| None),
            };
            match result {
                Ok(replacement) => {
                    generated.update(|items| {
                        items.retain(|s| s.name != secret.name);
                        items.extend(replacement);
                        items.sort_by(|a, b| a.name.cmp(&b.name));
                    });
                    feedback.succeed(action.success());
                }
                Err(e) => {
                    ready.set(false);
                    feedback.fail(
                        format!(
                            "{} Refresh, then retry if cleanup is pending.",
                            client_error_message(&e)
                        ),
                        &e,
                    );
                }
            }
            context.busy.set(false);
        });
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
                let unavailable = secret.unavailable;
                let deleting = secret.deleting;
                view! {
                    <tr>
                        <td>
                            <strong>{secret.name.clone()}</strong>
                        </td>
                        <td class="num">{secret.generation}</td>
                        <td class="muted">{when(secret.updated_at_ms)}</td>
                        <td>
                            {if deleting {
                                badge(Tone::Warn, "deletion pending")
                            } else if unavailable {
                                badge(Tone::Bad, "value discarded")
                            } else {
                                badge(Tone::Ok, "generated")
                            }}
                        </td>
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
                        .map(|s| s.name.to_string())
                        .collect::<Vec<_>>()
                }}
                key={|name| name.clone()}
                children={move |name| view! { <SecretFiles service_name={name} /> }}
            />
        </div>
    }
}

/// Editor for one service's secret file references (secret name and container
/// path), saved as part of the application configuration.
#[component]
fn SecretFiles(service_name: String) -> impl IntoView {
    let context = editor();
    let mounts = RwSignal::new(context.saved.with_untracked(|a| {
        a.application
            .spec()
            .services
            .iter()
            .find(|s| s.name.as_str() == service_name)
            .map(|s| s.secrets.clone())
            .unwrap_or_default()
    }));
    let baseline = RwSignal::new(mounts.get_untracked());
    dirty_group(format!("secret-files:{service_name}"), mounts, baseline);
    let service = StoredValue::new(service_name.clone());
    let save = move || {
        context.save(
            ApplicationEdit::Service {
                name: service.get_value(),
                edit: ServiceEdit::Secrets(mounts.get_untracked()),
            },
            Callback::new(move |saved: ApplicationView| {
                if let Some(target) = saved
                    .application
                    .spec()
                    .services
                    .iter()
                    .find(|s| s.name.as_str() == service.get_value())
                {
                    mounts.set(target.secrets.clone());
                    baseline.set(target.secrets.clone());
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
                    each={move || (0..mounts.get().len()).collect::<Vec<_>>()}
                    key={|i| *i}
                    children={move |index| {
                        view! {
                            <div class="form-row">
                                <label class="field">
                                    <span>"Secret name"</span>
                                    <input
                                        prop:value={move || {
                                            mounts
                                                .get()
                                                .get(index)
                                                .map(|m| m.name.to_string())
                                                .unwrap_or_default()
                                        }}
                                        on:input={move |e| {
                                            mounts
                                                .update(|items| items[index].name = event_target_value(&e).into());
                                        }}
                                    />
                                </label>
                                <label class="field">
                                    <span>"Container path"</span>
                                    <input
                                        prop:value={move || {
                                            mounts
                                                .get()
                                                .get(index)
                                                .map(|m| m.target.clone())
                                                .unwrap_or_default()
                                        }}
                                        on:input={move |e| {
                                            mounts
                                                .update(|items| {
                                                    items[index].target = event_target_value(&e);
                                                });
                                        }}
                                    />
                                </label>
                                {remove_button(move || {
                                    mounts
                                        .update(|items| {
                                            items.remove(index);
                                        });
                                })}
                            </div>
                        }
                    }}
                />
            </div>
            <button
                type="button"
                class="btn btn-sm"
                on:click={move |_| {
                    mounts
                        .update(|items| {
                            items
                                .push(piqueld_client::SecretMount {
                                    name: piqueld_client::Template::default(),
                                    target: "/run/secrets/".into(),
                                });
                        });
                }}
            >
                {icon(Icon::Plus)}
                "Add secret file"
            </button>
            {save_actions(mounts, baseline, save, || false)}
        </section>
    }
}
