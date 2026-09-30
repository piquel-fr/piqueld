//! Write-only secret values and saved file references.
use super::super::ui::{Icon, Tone, badge, empty, icon, notice, text_input, when};
use super::{client_error_message, diagnostic_id, dirty_group, editor, save_actions};
use leptos::{
    Callable, Callback, CollectView, For, IntoView, SignalGet, SignalGetUntracked, SignalSet,
    SignalUpdate, SignalWith, SignalWithUntracked, component, create_rw_signal, event_target_value,
    spawn_local, store_value, view, window,
};
use leptos_router::A;
use piqueld_client::{
    ApplicationView, Client,
    edit::{ApplicationEdit, ServiceEdit},
};

/// Secrets tab. Loads secret metadata (never values), writes or deletes secrets
/// guarded by their generation, and clears the value field as soon as it is
/// submitted. After a failed write or delete, actions stay disabled until the
/// metadata is refreshed. Also renders `SecretFiles` for every service.
#[component]
pub(super) fn ApplicationSecrets() -> impl IntoView {
    let context = editor();
    let metadata = create_rw_signal(Vec::<piqueld_client::SecretMetadata>::new());
    let ready = create_rw_signal(false);
    let loading = create_rw_signal(false);
    let error = create_rw_signal(None::<String>);
    let diagnostic = create_rw_signal(None::<String>);
    let fail = move |message: String, e: &piqueld_client::ClientError| {
        diagnostic.set(diagnostic_id(e));
        error.set(Some(message));
    };
    let clear = move || {
        diagnostic.set(None);
        error.set(None);
    };
    let notice_text = create_rw_signal(String::new());
    let name = create_rw_signal(String::new());
    let value = create_rw_signal(String::new());
    let empty_value = create_rw_signal(String::new());
    dirty_group("secret-value".into(), value, empty_value);
    let id = store_value(context.id());
    let reload = Callback::new(move |()| {
        if context.blocked() || loading.get_untracked() {
            return;
        }
        loading.set(true);
        ready.set(false);
        let id = id.get_value();
        spawn_local(async move {
            match Client::browser().secrets(&id).await {
                Ok(items) => {
                    metadata.set(items);
                    ready.set(true);
                    clear();
                }
                Err(e) => fail(client_error_message(&e), &e),
            }
            loading.set(false);
        });
    });
    reload.call(());
    let write = move |_| {
        if context.blocked() || !ready.get_untracked() {
            return;
        }
        let name = name.get_untracked();
        let generation = metadata.with_untracked(|items| {
            items
                .iter()
                .find(|s| s.name == name)
                .map_or(0, |s| s.generation)
        });
        if generation>0 && !window().confirm_with_message(&format!("Replace {name}? Running deployments keep their current version. A later Deploy will use the replacement.")).unwrap_or(false){return;}
        let bytes = value.get_untracked().into_bytes();
        value.set(String::new());
        let id = id.get_value();
        context.busy.set(true);
        notice_text.set(String::new());
        clear();
        spawn_local(async move {
            match Client::browser()
                .put_secret(&id, &name, generation, bytes)
                .await
            {
                Ok(secret) => {
                    metadata.update(|items| {
                        items.retain(|s| s.name != secret.name);
                        items.push(secret);
                        items.sort_by(|a, b| a.name.cmp(&b.name));
                    });
                    notice_text
                        .set("Secret saved. Deploy the application to use its new version.".into());
                }
                Err(e) => {
                    ready.set(false);
                    fail(
                        format!(
                            "{} Refresh metadata before another secret change. The submitted value has been cleared.",
                            client_error_message(&e)
                        ),
                        &e,
                    );
                }
            }
            context.busy.set(false);
        });
    };
    let remove = Callback::new(move |secret: piqueld_client::SecretMetadata| {
        if context.blocked() || !ready.get_untracked() {
            return;
        }
        if !window()
            .confirm_with_message(&format!(
                "Delete {} and its retained versions? Referenced secrets cannot be deleted.",
                secret.name
            ))
            .unwrap_or(false)
        {
            return;
        }
        let id = id.get_value();
        context.busy.set(true);
        notice_text.set(String::new());
        clear();
        spawn_local(async move {
            match Client::browser()
                .delete_secret(&id, &secret.name, secret.generation)
                .await
            {
                Ok(()) => {
                    metadata.update(|items| items.retain(|s| s.name != secret.name));
                    notice_text.set("Secret deleted.".into());
                    clear();
                }
                Err(e) => {
                    ready.set(false);
                    fail(
                        format!(
                            "{} Refresh metadata, then retry deletion if cleanup is pending.",
                            client_error_message(&e)
                        ),
                        &e,
                    );
                }
            }
            context.busy.set(false);
        });
    });
    let secret_rows = move || {
        metadata
            .get()
            .into_iter()
            .map(|secret| {
                let selected = secret.name.clone();
                let deleting = secret.deleting;
                let unavailable = secret.unavailable;
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
                                badge(Tone::Ok, "stored")
                            }}
                        </td>
                        <td class="actions">
                            <span class="btn-group" style="justify-content:flex-end">
                                <button
                                    type="button"
                                    class="btn btn-sm"
                                    disabled={move || context.blocked() || !ready.get() || deleting}
                                    on:click={move |_| name.set(selected.clone())}
                                >
                                    "Replace"
                                </button>
                                <button
                                    type="button"
                                    class="btn btn-ghost btn-sm"
                                    disabled={move || context.blocked() || !ready.get()}
                                    on:click={move |_| remove.call(secret.clone())}
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
    view! {
        <div class="stack">
            <section class="card">
                <header>
                    <div>
                        <h3>"Secrets"</h3>
                        <p>
                            "Values are write-only and never read back. Replace a value, then deploy to adopt the new version; running deployments keep theirs."
                        </p>
                    </div>
                    <button
                        type="button"
                        class="btn btn-sm"
                        disabled={move || context.blocked() || loading.get()}
                        on:click={move |_| reload.call(())}
                    >
                        {icon(Icon::Refresh)}
                        "Refresh"
                    </button>
                </header>
                <div class="stack-sm">
                    {move || {
                        error
                            .get()
                            .map(|e| {
                                notice(
                                    Tone::Bad,
                                    view! {
                                        <span>{e}</span>
                                        {diagnostic
                                            .get()
                                            .map(|id| {
                                                view! {
                                                    <A href={format!("/dashboard/errors/{id}")}>"Diagnostic details"</A>
                                                }
                                            })}
                                    },
                                )
                            })
                    }}
                    {move || (!notice_text.get().is_empty()).then(|| notice(Tone::Ok, notice_text.get()))}
                    {move || {
                        metadata
                            .with(|items| items.iter().any(|s| s.unavailable))
                            .then(|| {
                                notice(
                                    Tone::Warn,
                                    "Some values were discarded by secret key recovery. Supply a replacement value for each, then deploy.",
                                )
                            })
                    }}
                    <div class="table-wrap">
                        {move || {
                            if metadata.with(Vec::is_empty) {
                                empty(if ready.get() { "No secrets stored for this application." } else { "Loading secrets…" })
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
                                        <tbody>{secret_rows()}</tbody>
                                    </table>
                                }
                                    .into_view()
                            }
                        }}
                    </div>
                    <fieldset class="stack-sm" disabled={move || context.blocked() || !ready.get()}>
                        <div class="section-header">
                            <h4>"Save a value"</h4>
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
                        <div class="form-actions">
                            <button
                                type="button"
                                class="btn btn-primary"
                                disabled={move || {
                                    name.get().is_empty() || value.get().is_empty()
                                        || metadata.get().iter().any(|s| s.name == name.get() && s.deleting)
                                }}
                                on:click={write}
                            >
                                "Save secret"
                            </button>
                        </div>
                    </fieldset>
                </div>
            </section>
            {move || {
                context
                    .managed()
                    .then(|| {
                        notice(
                            Tone::Info,
                            "Secret file references are managed in the repository manifest.",
                        )
                    })
            }}
            <fieldset class="stack" disabled={move || context.blocked() || context.managed()}>
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
            </fieldset>
        </div>
    }
}

/// Editor for one service's secret file references (secret name and container
/// path), saved as part of the application configuration.
#[component]
fn SecretFiles(service_name: String) -> impl IntoView {
    let context = editor();
    let mounts = create_rw_signal(context.saved.with_untracked(|a| {
        a.application
            .spec()
            .services
            .iter()
            .find(|s| s.name.as_str() == service_name)
            .map(|s| s.secrets.clone())
            .unwrap_or_default()
    }));
    let baseline = create_rw_signal(mounts.get_untracked());
    dirty_group(format!("secret-files:{service_name}"), mounts, baseline);
    let service = store_value(service_name.clone());
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
                        "Mount stored secrets as files under /run/secrets. This saves references only; deploy to mount the referenced versions."
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
                                            mounts.get().get(index).map(|m| m.name.clone()).unwrap_or_default()
                                        }}
                                        on:input={move |e| {
                                            mounts.update(|items| items[index].name = event_target_value(&e));
                                        }}
                                    />
                                </label>
                                <label class="field">
                                    <span>"Container path"</span>
                                    <input
                                        prop:value={move || {
                                            mounts.get().get(index).map(|m| m.target.clone()).unwrap_or_default()
                                        }}
                                        on:input={move |e| {
                                            mounts.update(|items| items[index].target = event_target_value(&e));
                                        }}
                                    />
                                </label>
                                <button
                                    type="button"
                                    class="btn btn-ghost"
                                    on:click={move |_| {
                                        mounts.update(|items| {
                                            items.remove(index);
                                        });
                                    }}
                                >
                                    "Remove"
                                </button>
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
                                    name: String::new(),
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
