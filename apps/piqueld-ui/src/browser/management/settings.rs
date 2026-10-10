//! Saved configuration forms and service editors.
use super::super::client_error_message;
use super::super::ui::{Icon, Modal, Tone, icon, notice, remove_button, text_input, when};
use super::{dirty_group, editor, save_actions};
use crate::editor::{Section, ServiceForm};
use leptos::prelude::*;
use piqueld_client::{
    ApplicationView, Client, GitRepository, Mount, RepositoryManifest, Rollout, Service, Source,
    Typed, Volume,
    edit::{ApplicationEdit, ServiceEdit, ServiceGeneral, ServiceProcess},
    sync::RepositorySync,
};
use std::collections::BTreeMap;

/// Toggle and fields for loading the application's configuration from a Git
/// manifest on deploy, and for deploying it on push. The draft is `(enabled,
/// manifest, poll interval as typed)`; saving is refused while other form
/// groups have unsaved edits.
#[component]
pub(super) fn RepositorySettings() -> impl IntoView {
    let context = editor();
    let backing = context.manifest().spec.manifest;
    let enabled = backing.is_some();
    let manifest = backing.unwrap_or(RepositoryManifest {
        repository: GitRepository {
            url: String::new(),
            branch: "main".into(),
            commit: None,
        },
        path: "infra/piqueld/app.toml".into(),
        sync: RepositorySync::Off,
    });
    let interval = match manifest.sync {
        RepositorySync::Poll { interval_seconds } => interval_seconds,
        RepositorySync::Off | RepositorySync::Webhook => RepositorySync::DEFAULT_INTERVAL,
    };
    let draft = RwSignal::new((enabled, manifest, interval.to_string()));
    let baseline = RwSignal::new(draft.get_untracked());
    // Once connected, each environment follows its own branch, changed on its page.
    let connected = move || baseline.get().0;
    dirty_group("repository".into(), draft, baseline);
    let other_edits = move || {
        context
            .dirty
            .with(|groups| groups.iter().any(|group| group != "repository"))
    };
    // How the saved connection syncs; `None` while disconnected.
    let saved_sync = move || {
        context.saved.with(|saved| {
            saved
                .application
                .spec()
                .manifest
                .as_ref()
                .map(|manifest| manifest.sync)
        })
    };
    let polling = move || matches!(draft.get().1.sync, RepositorySync::Poll { .. });
    let save = move || {
        if other_edits() {
            return;
        }
        let value = draft.get_untracked();
        // A valid typed interval is already in `sync`; only a malformed one is not.
        if matches!(value.1.sync, RepositorySync::Poll { .. })
            && value.2.trim().parse::<u32>().is_err()
        {
            context.set_error(Some(
                "The poll interval must be a whole number of seconds.".into(),
            ));
            return;
        }
        context.save(
            ApplicationEdit::Repository(value.0.then_some(value.1)),
            Callback::new(move |_| baseline.set(draft.get_untracked())),
        );
    };
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>"Repository manifest"</h3>
                    <p>
                        "Load the complete application manifest from Git on every deploy. The repository and manifest path are shared by every environment; each environment follows its own branch, set when connecting or on its page. Runtime configuration moves to the repository."
                    </p>
                </div>
            </header>
            <div class="stack-sm">
                <Show when={other_edits}>
                    {notice(
                        Tone::Warn,
                        "Save or discard edits in other settings before changing the repository connection.",
                    )}
                </Show>
                <fieldset class="stack-sm" disabled={move || context.blocked()}>
                    <label class="checkbox">
                        <input
                            type="checkbox"
                            prop:checked={move || draft.get().0}
                            on:change={move |ev| draft.update(|v| v.0 = event_target_checked(&ev))}
                        />
                        "Load configuration from Git on deploy"
                    </label>
                    <div hidden={move || !draft.get().0} class="form-grid">
                        {text_input(
                            "Repository",
                            draft,
                            |v| v.1.repository.url.clone(),
                            |v, s| v.1.repository.url = s,
                        )}
                        <Show when={move || !connected()}>
                            {text_input(
                                "Branch",
                                draft,
                                |v| v.1.repository.branch.clone(),
                                |v, s| v.1.repository.branch = s,
                            )}
                            {text_input(
                                "Commit (optional)",
                                draft,
                                |v| v.1.repository.commit.clone().unwrap_or_default(),
                                |v, s| v.1.repository.commit = (!s.is_empty()).then_some(s),
                            )}
                        </Show>
                        {text_input(
                            "Manifest path",
                            draft,
                            |v| v.1.path.clone(),
                            |v, s| v.1.path = s,
                        )}
                        <label class="field">
                            <span>"Deploy on push"</span>
                            <select
                                prop:value={move || match draft.get().1.sync {
                                    RepositorySync::Off => "off",
                                    RepositorySync::Poll { .. } => "poll",
                                    RepositorySync::Webhook => "webhook",
                                }}
                                on:change={move |event| {
                                    let mode = event_target_value(&event);
                                    draft
                                        .update(|v| {
                                            v.1.sync = match mode.as_str() {
                                                "poll" => RepositorySync::Poll {
                                                    interval_seconds: v
                                                        .2
                                                        .trim()
                                                        .parse()
                                                        .unwrap_or(RepositorySync::DEFAULT_INTERVAL),
                                                },
                                                "webhook" => RepositorySync::Webhook,
                                                _ => RepositorySync::Off,
                                            };
                                        });
                                }}
                            >
                                <option value="off">"Off: deploy only when asked"</option>
                                <option value="poll">"Poll: check the branches on an interval"</option>
                                <option value="webhook">"Webhook: when GitHub reports a push"</option>
                            </select>
                        </label>
                        <Show when={polling}>
                            <label class="field">
                                <span>"Poll interval (seconds)"</span>
                                <input
                                    type="number"
                                    min={RepositorySync::MIN_INTERVAL}
                                    max={RepositorySync::MAX_INTERVAL}
                                    prop:value={move || draft.with(|v| v.2.clone())}
                                    on:input={move |event| {
                                        let text = event_target_value(&event);
                                        draft
                                            .update(|v| {
                                                if let (
                                                    RepositorySync::Poll { interval_seconds },
                                                    Ok(seconds),
                                                ) = (&mut v.1.sync, text.trim().parse())
                                                {
                                                    *interval_seconds = seconds;
                                                }
                                                v.2 = text;
                                            });
                                    }}
                                />
                            </label>
                        </Show>
                    </div>
                    <p class="hint" hidden={move || !draft.get().0}>
                        "When on, pushes deploy every preview, and every environment that opted in on its page and follows an unpinned branch, once it was deployed."
                    </p>
                    {save_actions(draft, baseline, save, other_edits)}
                </fieldset>
                <Show when={move || saved_sync().is_some_and(|sync| !sync.is_off())}>
                    <SyncCheck />
                </Show>
            </div>
        </section>
        <Show when={move || saved_sync() == Some(RepositorySync::Webhook)}>
            <WebhookSettings />
        </Show>
    }
}

/// When sync last listed the repository's branches, and why it failed.
#[component]
fn SyncCheck() -> impl IntoView {
    let context = editor();
    // Loaded when shown and on Refresh: dashboard polling only reloads the
    // application when its configuration changes, which a check does not.
    let loaded = LocalResource::new(move || {
        let id = context.id();
        async move {
            Client::browser()
                .application(&id)
                .await
                .map(|application| application.sync_check)
                .ok()
        }
    });
    let check = move || {
        loaded
            .get()
            .flatten()
            .unwrap_or_else(|| context.saved.with(|saved| saved.sync_check.clone()))
    };
    view! {
        <dl class="kv">
            <dt>"Last sync check"</dt>
            <dd>
                {move || {
                    check()
                        .map_or_else(
                            || view! { <span class="muted">"Not yet"</span> }.into_any(),
                            |check| when(check.checked_at_ms),
                        )
                }}
                " "
                <button type="button" class="btn btn-sm" on:click={move |_| loaded.refetch()}>
                    {icon(Icon::Refresh)}
                    "Refresh"
                </button>
            </dd>
        </dl>
        {move || {
            check()
                .and_then(|check| check.error)
                .map(|error| {
                    notice(
                        Tone::Bad,
                        format!(
                            "Sync could not list the repository's branches: {error}. It retries with backoff.",
                        ),
                    )
                })
        }}
    }
}

/// What to configure in GitHub for an application syncing on push webhooks:
/// the payload URL, and a secret generated on demand and shown only once.
/// Rotating confirms first, since the previous secret stops verifying at once.
#[component]
fn WebhookSettings() -> impl IntoView {
    let context = editor();
    let data = LocalResource::new(move || {
        let id = context.id();
        async move {
            Client::browser()
                .webhook(&id)
                .await
                .map_err(|error| client_error_message(&error))
        }
    });
    // The secret just generated: never retrievable again.
    let generated = RwSignal::new(None::<String>);
    let created = move || {
        data.get()
            .and_then(Result::ok)
            .and_then(|webhook| webhook.secret_created_at_ms)
    };
    let generate = move |_| {
        if created().is_some()
            && !window()
                .confirm_with_message(
                    "Rotate the webhook secret? The current secret stops verifying deliveries at once, until GitHub is updated with the new one.",
                )
                .unwrap_or(false)
        {
            return;
        }
        let application = context.id();
        context.mutate(
            move |client| {
                let application = application.clone();
                async move { client.generate_webhook_secret(&application).await }
            },
            move |secret| {
                generated.set(Some(secret.secret));
                data.refetch();
            },
        );
    };
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>"GitHub webhook"</h3>
                    <p>
                        "Add a webhook to the repository in GitHub with these settings. Each push makes piqueld list the branches itself; the payload is only authenticated, never trusted."
                    </p>
                </div>
                <button
                    type="button"
                    class="btn"
                    disabled={move || context.blocked() || data.get().is_none()}
                    on:click={generate}
                >
                    {icon(Icon::Key)}
                    {move || if created().is_some() { "Rotate secret" } else { "Generate secret" }}
                </button>
            </header>
            <div class="stack-sm">
                {move || match data.get() {
                    None => view! { <p class="hint">"Loading…"</p> }.into_any(),
                    Some(Err(error)) => notice(Tone::Bad, error),
                    Some(Ok(webhook)) => {
                        view! {
                            {webhook
                                .url
                                .is_none()
                                .then(|| {
                                    notice(
                                        Tone::Warn,
                                        "This daemon does not receive webhooks: set ingress.webhook_hostname in its configuration to get a payload URL.",
                                    )
                                })}
                            <dl class="kv">
                                <dt>"Payload URL"</dt>
                                <dd>
                                    {webhook
                                        .url
                                        .map_or_else(
                                            || view! { <span class="muted">"None"</span> }.into_any(),
                                            |url| view! { <code>{url}</code> }.into_any(),
                                        )}
                                </dd>
                                <dt>"Content type"</dt>
                                <dd>
                                    <code>"application/json"</code>
                                </dd>
                                <dt>"Events"</dt>
                                <dd>"Just the push event"</dd>
                                <dt>"Secret"</dt>
                                <dd>
                                    {webhook
                                        .secret_created_at_ms
                                        .map_or_else(
                                            || {
                                                view! { <span class="muted">"None yet: generate one"</span> }
                                                    .into_any()
                                            },
                                            |at| view! { "Generated " {when(at)} }.into_any(),
                                        )}
                                </dd>
                            </dl>
                        }
                            .into_any()
                    }
                }}
                {move || {
                    generated
                        .get()
                        .map(|secret| {
                            notice(
                                Tone::Ok,
                                view! {
                                    "Copy this secret into GitHub now; it will not be shown again."
                                    <pre class="secret-box">{secret}</pre>
                                },
                            )
                        })
                }}
            </div>
        </section>
    }
}

/// Application title with an inline rename form behind the pencil button.
#[component]
pub(super) fn MetadataSettings() -> impl IntoView {
    let context = editor();
    let editing = RwSignal::new(false);
    let name = RwSignal::new(context.name());
    let baseline = RwSignal::new(name.get_untracked());
    dirty_group("name".into(), name, baseline);
    let save = move |()| {
        context.save(
            ApplicationEdit::Name(name.get_untracked()),
            Callback::new(move |app: ApplicationView| {
                let saved_name = app.application.metadata().name.to_string();
                name.set(saved_name.clone());
                baseline.set(saved_name);
                editing.set(false);
            }),
        );
    };
    view! {
        <h1 hidden={move || editing.get()}>{move || context.name()}</h1>
        <button
            type="button"
            class="btn btn-ghost btn-icon"
            aria-label="Rename application"
            title="Rename application"
            hidden={move || editing.get()}
            disabled={move || context.blocked()}
            on:click={move |_| editing.set(true)}
        >
            {icon(Icon::Pencil)}
        </button>
        <form
            class="form-row"
            hidden={move || !editing.get()}
            on:submit={move |event| {
                event.prevent_default();
                save(());
            }}
        >
            <fieldset class="form-row" disabled={move || context.blocked()}>
                {text_input("Application name", name, String::clone, |v, s| *v = s)}
                <button
                    type="submit"
                    class="btn btn-primary"
                    disabled={move || name.get() == baseline.get()}
                >
                    "Save"
                </button>
                <button
                    type="button"
                    class="btn btn-ghost"
                    on:click={move |_| {
                        name.set(baseline.get_untracked());
                        editing.set(false);
                    }}
                >
                    "Cancel"
                </button>
            </fieldset>
        </form>
    }
}

/// Named volume list editor, saved as one group.
#[component]
pub(super) fn VolumeSettings() -> impl IntoView {
    let context = editor();
    let volumes = RwSignal::new(
        context
            .manifest()
            .spec
            .volumes
            .iter()
            .map(|v| v.name.clone())
            .collect::<Vec<_>>(),
    );
    let baseline = RwSignal::new(volumes.get_untracked());
    dirty_group("volumes".into(), volumes, baseline);
    let save = move || {
        let changed = volumes
            .get_untracked()
            .into_iter()
            .map(|name| Volume { name })
            .collect();
        context.save(
            ApplicationEdit::Volumes(changed),
            Callback::new(move |app: ApplicationView| {
                let names = app
                    .application
                    .to_manifest()
                    .spec
                    .volumes
                    .into_iter()
                    .map(|v| v.name)
                    .collect();
                volumes.set(names);
                baseline.set(volumes.get_untracked());
            }),
        );
    };
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>"Named volumes"</h3>
                    <p>
                        "Volumes persist across deployments. Mount them from a service’s Volume mounts tab. Removing a volume here keeps its Docker data."
                    </p>
                </div>
            </header>
            <fieldset disabled={move || context.blocked()}>
                <div class="form-list">
                    <For
                        each={move || (0..volumes.with(Vec::len)).collect::<Vec<_>>()}
                        key={|i| *i}
                        children={move |i| {
                            view! {
                                <div class="form-row">
                                    {text_input(
                                        "Volume name",
                                        volumes,
                                        move |v| v.get(i).cloned().unwrap_or_default(),
                                        move |v, s| {
                                            if let Some(value) = v.get_mut(i) {
                                                *value = s;
                                            }
                                        },
                                    )}
                                    {remove_button(move || {
                                        volumes
                                            .update(|v| {
                                                v.remove(i);
                                            })
                                    })}
                                </div>
                            }
                        }}
                    />
                </div>
                {add_button("Add volume", move || volumes.update(|v| v.push(String::new())))}
                {save_actions(volumes, baseline, save, || false)}
            </fieldset>
        </section>
    }
}

fn add_button(label: &'static str, add: impl Fn() + 'static) -> AnyView {
    view! {
        <button type="button" class="btn btn-sm" on:click={move |_| add()}>
            {icon(Icon::Plus)}
            {label}
        </button>
    }
    .into_any()
}

/// Form for one `Section` of service `name`. Saving patches only that section
/// onto a copy of the saved service (validating the text fields) and sends the
/// matching `ServiceEdit`, so other sections' saved values are left untouched.
#[component]
pub(super) fn ServiceGroup(name: String, section: Section) -> impl IntoView {
    let context = editor();
    let service = context
        .manifest()
        .spec
        .services
        .into_iter()
        .find(|s| s.name == name)
        .expect("listed service");
    let draft = RwSignal::new(ServiceForm::from(&service));
    let baseline = RwSignal::new(draft.get_untracked());
    dirty_group(format!("{name}:{}", section.title()), draft, baseline);
    let name = StoredValue::new(name);
    let save = move || {
        let mut manifest = context.manifest();
        let name = name.get_value();
        let Some(service) = manifest.spec.services.iter_mut().find(|s| s.name == name) else {
            context.set_error(Some("Service was removed. Reload configuration.".into()));
            return;
        };
        if let Err(error) = draft.get_untracked().patch(section, service) {
            context.set_error(Some(error));
            return;
        }
        let sent = draft.get_untracked();
        let edit = match section {
            Section::General => ServiceEdit::General(ServiceGeneral {
                source: service.source.clone(),
                replicas: service.replicas.clone(),
            }),
            Section::Environment => ServiceEdit::Environment(service.environment.clone()),
            Section::Process => ServiceEdit::Process(ServiceProcess {
                command: service.command.clone(),
                arguments: service.arguments.clone(),
            }),
            Section::Storage => ServiceEdit::Mounts(service.mounts.clone()),
            Section::Health => ServiceEdit::Healthcheck(service.healthcheck.clone()),
            Section::Dependencies => ServiceEdit::DependsOn(service.depends_on.clone()),
            Section::Rollout => ServiceEdit::Rollout(service.rollout.clone()),
            Section::Resources => ServiceEdit::Resources(service.resources.clone()),
        };
        context.save(
            ApplicationEdit::Service { name, edit },
            Callback::new(move |_| baseline.set(sent.clone())),
        );
    };
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>{section.title()}</h3>
                    <p>{section_hint(section)}</p>
                </div>
            </header>
            <fieldset disabled={move || {
                context.blocked() || context.managed()
            }}>
                {service_fields(section, name.get_value(), draft)} {save_actions(draft, baseline, save, || false)}
            </fieldset>
        </section>
    }
}

const fn section_hint(section: Section) -> &'static str {
    match section {
        Section::General => "Where the container image comes from and how many replicas run.",
        Section::Environment => "Environment variables passed to every replica.",
        Section::Process => {
            "Each row is one element. Spaces are preserved; no shell parsing is applied."
        }
        Section::Storage => "Mount declared volumes at absolute container paths.",
        Section::Health => {
            "Docker restarts unhealthy replicas and counts only healthy ones toward readiness."
        }
        Section::Dependencies => {
            "Deploy rolls this service out only after the selected services are healthy, or running when they have no health check."
        }
        Section::Rollout => {
            "Stop-first never runs two replicas on the same data but is briefly unavailable; start-first avoids downtime. Derived stops first only when a volume is mounted writable."
        }
        Section::Resources => "Leave a limit blank to use the runtime default.",
    }
}

/// Input fields for one `Section` of `service`, bound to the shared draft form.
pub(super) fn service_fields(
    section: Section,
    service: String,
    form: RwSignal<ServiceForm>,
) -> AnyView {
    match section {
        Section::General => {
            let kind = Memo::new(move |_| form.with(|form| form.source_kind.clone()));
            view! {
                <div class="form-grid">
                    <label class="field">
                        <span>"Source"</span>
                        <select
                            prop:value={move || form.get().source_kind}
                            on:change={move |ev| {
                                form.update(|v| v.source_kind = event_target_value(&ev));
                            }}
                        >
                            <option value="image">"Container image"</option>
                            <option value="git">"Git repository with Dockerfile"</option>
                            <option value="self">"Manifest repository with Dockerfile"</option>
                        </select>
                    </label>
                    {text_input("Replicas", form, |v| v.replicas.clone(), |v, s| v.replicas = s)}
                    {move || {
                        if kind.get() == "image" {
                            text_input(
                                "Container image",
                                form,
                                |v| v.image.clone(),
                                |v, s| v.image = s,
                            )
                        } else {
                            view! {
                                <Show when={move || kind.get() == "git"}>
                                    {text_input(
                                        "Repository",
                                        form,
                                        |v| v.repository.clone(),
                                        |v, s| v.repository = s,
                                    )}
                                    {text_input(
                                        "Branch",
                                        form,
                                        |v| v.branch.clone(),
                                        |v, s| v.branch = s,
                                    )}
                                    {text_input(
                                        "Commit (optional)",
                                        form,
                                        |v| v.commit.clone(),
                                        |v, s| v.commit = s,
                                    )}
                                </Show>
                                {text_input(
                                    "Dockerfile path",
                                    form,
                                    |v| v.dockerfile.clone(),
                                    |v, s| v.dockerfile = s,
                                )}
                                {text_input(
                                    "Build context",
                                    form,
                                    |v| v.context.clone(),
                                    |v, s| v.context = s,
                                )}
                                {text_input(
                                    "Build target (optional)",
                                    form,
                                    |v| v.target.clone(),
                                    |v, s| v.target = s,
                                )}
                                <div style="grid-column:1/-1">
                                    <h4 class="hint" style="margin-bottom:8px">
                                        "Build arguments (not secret)"
                                    </h4>
                                    {pair_rows(
                                        "Add build argument",
                                        form,
                                        |v| &v.build_args,
                                        |v| &mut v.build_args,
                                    )}
                                </div>
                            }
                                .into_any()
                        }
                    }}
                </div>
            }
            .into_any()
        }
        Section::Environment => pair_rows(
            "Add variable",
            form,
            |v| &v.environment,
            |v| &mut v.environment,
        ),
        Section::Process => view! {
            <div class="stack-sm">
                <div>
                    <h4 class="hint" style="margin-bottom:8px">
                        "Command"
                    </h4>
                    {string_rows("Command element", form, |v| &v.command, |v| &mut v.command)}
                </div>
                <div>
                    <h4 class="hint" style="margin-bottom:8px">
                        "Arguments"
                    </h4>
                    {string_rows("Argument", form, |v| &v.arguments, |v| &mut v.arguments)}
                </div>
            </div>
        }
        .into_any(),
        Section::Storage => mount_fields(form),
        Section::Health => health_fields(form),
        Section::Dependencies => dependency_fields(service, form),
        Section::Rollout => rollout_fields(form),
        Section::Resources => view! {
            <div class="form-grid">
                {text_input("CPU (millicores)", form, |v| v.cpu.clone(), |v, s| v.cpu = s)}
                {text_input("Memory (bytes)", form, |v| v.memory.clone(), |v, s| v.memory = s)}
            </div>
        }
        .into_any(),
    }
}

/// Editable list of strings (one element per row, no shell parsing) selected
/// from the form by the `read`/`write` accessors.
pub(super) fn string_rows(
    label: &'static str,
    form: RwSignal<ServiceForm>,
    read: fn(&ServiceForm) -> &Vec<String>,
    write: fn(&mut ServiceForm) -> &mut Vec<String>,
) -> AnyView {
    view! {
        <div class="form-list">
            <For
                each={move || (0..form.with(|v| read(v).len())).collect::<Vec<_>>()}
                key={|i| *i}
                children={move |i| {
                    view! {
                        <div class="form-row">
                            {text_input(
                                label,
                                form,
                                move |v| read(v).get(i).cloned().unwrap_or_default(),
                                move |v, s| {
                                    if let Some(value) = write(v).get_mut(i) {
                                        *value = s;
                                    }
                                },
                            )}
                            {remove_button(move || {
                                form
                                    .update(|v| {
                                        write(v).remove(i);
                                    })
                            })}
                        </div>
                    }
                }}
            />
        </div>
        {add_button(label, move || form.update(|v| write(v).push(String::new())))}
    }
    .into_any()
}

/// One checkbox per other saved service; selections stay sorted like saved configuration.
pub(super) fn dependency_fields(service: String, form: RwSignal<ServiceForm>) -> AnyView {
    let context = editor();
    let candidates = move || {
        context
            .manifest()
            .spec
            .services
            .into_iter()
            .map(|candidate| candidate.name)
            .filter(|candidate| *candidate != service)
            .collect::<Vec<_>>()
    };
    view! {
        <div class="form-list">
            <For
                each={candidates}
                key={Clone::clone}
                children={move |candidate| {
                    let checked = candidate.clone();
                    let toggled = candidate.clone();
                    view! {
                        <label class="checkbox">
                            <input
                                type="checkbox"
                                prop:checked={move || form.with(|v| v.depends_on.contains(&checked))}
                                on:change={move |ev| {
                                    let selected = event_target_checked(&ev);
                                    form.update(|v| {
                                        v.depends_on.retain(|name| *name != toggled);
                                        if selected {
                                            v.depends_on.push(toggled.clone());
                                            v.depends_on.sort();
                                        }
                                    });
                                }}
                            />
                            {candidate}
                        </label>
                    }
                }}
            />
        </div>
    }
    .into_any()
}

/// Editable key/value rows, such as environment variables or build arguments,
/// selected from the form by the `read`/`write` accessors.
pub(super) fn pair_rows(
    add_label: &'static str,
    form: RwSignal<ServiceForm>,
    read: fn(&ServiceForm) -> &Vec<(String, String)>,
    write: fn(&mut ServiceForm) -> &mut Vec<(String, String)>,
) -> AnyView {
    view! {
        <div class="form-list">
            <For
                each={move || (0..form.with(|v| read(v).len())).collect::<Vec<_>>()}
                key={|i| *i}
                children={move |i| {
                    view! {
                        <div class="form-row">
                            {text_input(
                                "Key",
                                form,
                                move |v| read(v).get(i).map_or_else(String::new, |v| v.0.clone()),
                                move |v, s| {
                                    if let Some(value) = write(v).get_mut(i) {
                                        value.0 = s;
                                    }
                                },
                            )}
                            {text_input(
                                "Value",
                                form,
                                move |v| read(v).get(i).map_or_else(String::new, |v| v.1.clone()),
                                move |v, s| {
                                    if let Some(value) = write(v).get_mut(i) {
                                        value.1 = s;
                                    }
                                },
                            )}
                            {remove_button(move || {
                                form
                                    .update(|v| {
                                        write(v).remove(i);
                                    })
                            })}
                        </div>
                    }
                }}
            />
        </div>
        {add_button(add_label, move || form.update(|v| write(v).push(Default::default())))}
    }
    .into_any()
}
/// Volume mount rows: volume name, container path and read-only flag.
pub(super) fn mount_fields(form: RwSignal<ServiceForm>) -> AnyView {
    view! {
        <div class="form-list">
            <For
                each={move || (0..form.with(|v| v.mounts.len())).collect::<Vec<_>>()}
                key={|i| *i}
                children={move |i| {
                    view! {
                        <div class="form-row">
                            {text_input(
                                "Volume",
                                form,
                                move |v| {
                                    v.mounts.get(i).map_or_else(String::new, |v| v.volume.clone())
                                },
                                move |v, s| {
                                    if let Some(value) = v.mounts.get_mut(i) {
                                        value.volume = s;
                                    }
                                },
                            )}
                            {text_input(
                                "Container path",
                                form,
                                move |v| {
                                    v.mounts.get(i).map_or_else(String::new, |v| v.target.clone())
                                },
                                move |v, s| {
                                    if let Some(value) = v.mounts.get_mut(i) {
                                        value.target = s;
                                    }
                                },
                            )} <label class="checkbox">
                                <input
                                    type="checkbox"
                                    prop:checked={move || {
                                        form.with(|v| v.mounts.get(i).is_some_and(|v| v.read_only))
                                    }}
                                    on:change={move |event| {
                                        form.update(|v| {
                                            if let Some(value) = v.mounts.get_mut(i) {
                                                value.read_only = event_target_checked(&event);
                                            }
                                        });
                                    }}
                                />
                                "Read only"
                            </label>
                            {remove_button(move || {
                                form
                                    .update(|v| {
                                        v.mounts.remove(i);
                                    })
                            })}
                        </div>
                    }
                }}
            />
        </div>
        {add_button(
            "Add mount",
            move || {
                form.update(|v| {
                    v.mounts
                        .push(Mount {
                            volume: String::new(),
                            target: String::new(),
                            read_only: false,
                        });
                });
            },
        )}
    }
    .into_any()
}
/// Health check type selector with the interval, timeout, and HTTP or command
/// fields relevant to the chosen type.
pub(super) fn health_fields(form: RwSignal<ServiceForm>) -> AnyView {
    view! {
        <div class="form-grid">
            <label class="field">
                <span>"Check type"</span>
                <select
                    prop:value={move || form.with(|v| v.health_kind.clone())}
                    on:change={move |event| {
                        form.update(|v| v.health_kind = event_target_value(&event))
                    }}
                >
                    <option value="none" selected={move || form.with(|v| v.health_kind == "none")}>
                        "None"
                    </option>
                    <option value="http" selected={move || form.with(|v| v.health_kind == "http")}>
                        "HTTP"
                    </option>
                    <option
                        value="command"
                        selected={move || form.with(|v| v.health_kind == "command")}
                    >
                        "Command"
                    </option>
                </select>
            </label>
            <Show when={move || {
                form.with(|v| v.health_kind != "none")
            }}>
                {text_input(
                    "Interval (seconds)",
                    form,
                    |v| v.interval.clone(),
                    |v, s| v.interval = s,
                )}
                {text_input("Timeout (seconds)", form, |v| v.timeout.clone(), |v, s| v.timeout = s)}
            </Show>
            <Show when={move || {
                form.with(|v| v.health_kind == "http")
            }}>
                {text_input("Port", form, |v| v.port.clone(), |v, s| v.port = s)}
                {text_input("Path", form, |v| v.path.clone(), |v, s| v.path = s)}
            </Show>
        </div>
        <Show when={move || form.with(|v| v.health_kind == "command")}>
            <div style="margin-top:14px">
                {string_rows(
                    "Health command element",
                    form,
                    |v| &v.health_command,
                    |v| &mut v.health_command,
                )}
            </div>
        </Show>
    }
    .into_any()
}

/// Rollout order and optional monitor window. The order is free text with
/// suggestions, so it can also hold a `${{ }}` reference.
pub(super) fn rollout_fields(form: RwSignal<ServiceForm>) -> AnyView {
    view! {
        <div class="form-grid">
            <label class="field">
                <span>"Order (derived, stop-first, start-first, or ${{ }})"</span>
                <input
                    type="text"
                    list="rollout-orders"
                    prop:value={move || form.with(|v| v.rollout_order.clone())}
                    on:input={move |event| {
                        form.update(|v| v.rollout_order = event_target_value(&event))
                    }}
                />
                <datalist id="rollout-orders">
                    <option value="derived">"Derived from mounts"</option>
                    <option value="stop-first">"Stop first"</option>
                    <option value="start-first">"Start first"</option>
                </datalist>
            </label>
            {text_input(
                "Monitor (seconds, default 30)",
                form,
                |v| v.monitor.clone(),
                |v, s| v.monitor = s,
            )}
        </div>
    }
    .into_any()
}

/// "Add service" button and modal that saves a new single-replica image service.
#[component]
pub(super) fn NewService() -> impl IntoView {
    let context = editor();
    let opened = RwSignal::new(false);
    let fields = RwSignal::new((String::new(), String::new()));
    let baseline = RwSignal::new(fields.get_untracked());
    dirty_group("new-service".into(), fields, baseline);
    let save = move |()| {
        let (name, image) = fields.get_untracked();
        let service = Service {
            secrets: Vec::new(),
            name,
            source: Source::Image {
                image: image.into(),
            },
            replicas: Typed::Literal(1),
            environment: BTreeMap::new(),
            command: Vec::new(),
            arguments: Vec::new(),
            mounts: Vec::new(),
            healthcheck: None,
            resources: None,
            depends_on: Vec::new(),
            rollout: Rollout::default(),
        };
        context.save(
            ApplicationEdit::AddService(Box::new(service)),
            Callback::new(move |_| {
                fields.set((String::new(), String::new()));
                opened.set(false);
            }),
        );
    };
    view! {
        <button
            type="button"
            class="btn btn-primary"
            disabled={move || context.blocked()}
            on:click={move |_| opened.set(true)}
        >
            {icon(Icon::Plus)}
            "Add service"
        </button>
        <Modal
            title="Add service"
            opened={opened}
            busy={context.busy}
            on_close={Callback::new(move |()| {
                fields.set(baseline.get_untracked());
                context.set_error(None);
            })}
        >
            <form
                class="stack-sm"
                on:submit={move |event| {
                    event.prevent_default();
                    save(());
                }}
            >
                <fieldset class="stack-sm" disabled={move || context.blocked()}>
                    <p class="hint">
                        "Starts with one replica of a prebuilt image. Switch to a Git build and tune everything else on the service page."
                    </p>
                    {text_input("Service name", fields, |v| v.0.clone(), |v, s| v.0 = s)}
                    {text_input("Container image", fields, |v| v.1.clone(), |v, s| v.1 = s)}
                </fieldset>
                {move || context.error.get().map(|error| notice(Tone::Bad, error))}
                <div class="form-actions">
                    <button
                        type="submit"
                        class="btn btn-primary"
                        disabled={move || context.blocked()}
                    >
                        "Add service"
                    </button>
                </div>
            </form>
        </Modal>
    }
}
