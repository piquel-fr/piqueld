//! Saved configuration forms and service editors.
use super::super::ui::{Icon, Modal, Tone, icon, notice, remove_button, text_input};
use super::{dirty_group, editor, save_actions};
use crate::editor::{Section, ServiceForm};
use leptos::{
    Callback, For, IntoView, RwSignal, Show, SignalGet, SignalGetUntracked, SignalSet,
    SignalUpdate, SignalWith, View, component, create_memo, create_rw_signal, event_target_checked,
    event_target_value, store_value, view,
};
use piqueld_client::{
    ApplicationView, GitRepository, Mount, RepositoryManifest, Service, Source, Volume,
    edit::{ApplicationEdit, ServiceEdit, ServiceGeneral, ServiceProcess},
};
use std::collections::BTreeMap;

/// Toggle and fields for loading the application's configuration from a Git
/// manifest on deploy. The draft is `(enabled, manifest)`; saving is refused
/// while other form groups have unsaved edits.
#[component]
pub(super) fn RepositorySettings() -> impl IntoView {
    let context = editor();
    let backing = context.manifest().spec.manifest;
    let draft = create_rw_signal((
        backing.is_some(),
        backing.unwrap_or(RepositoryManifest {
            repository: GitRepository {
                url: String::new(),
                branch: "main".into(),
                commit: None,
            },
            path: "infra/piqueld/app.toml".into(),
        }),
    ));
    let baseline = create_rw_signal(draft.get_untracked());
    dirty_group("repository".into(), draft, baseline);
    let other_edits = move || {
        context
            .dirty
            .with(|groups| groups.iter().any(|group| group != "repository"))
    };
    let save = move || {
        if other_edits() {
            return;
        }
        let value = draft.get_untracked();
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
                        "Load the complete application manifest from Git on every deploy. Connection settings stay editable here; runtime configuration moves to the repository."
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
                        {text_input("Manifest path", draft, |v| v.1.path.clone(), |v, s| v.1.path = s)}
                    </div>
                    {save_actions(draft, baseline, save, other_edits)}
                </fieldset>
            </div>
        </section>
    }
}

/// Application title with an inline rename form behind the pencil button.
#[component]
pub(super) fn MetadataSettings() -> impl IntoView {
    let context = editor();
    let editing = create_rw_signal(false);
    let name = create_rw_signal(context.name());
    let baseline = create_rw_signal(name.get_untracked());
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
    let volumes = create_rw_signal(
        context
            .manifest()
            .spec
            .volumes
            .iter()
            .map(|v| v.name.clone())
            .collect::<Vec<_>>(),
    );
    let baseline = create_rw_signal(volumes.get_untracked());
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
                                    {remove_button(move || volumes.update(|v| { v.remove(i); }))}
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

fn add_button(label: &'static str, add: impl Fn() + 'static) -> View {
    view! {
        <button type="button" class="btn btn-sm" on:click={move |_| add()}>
            {icon(Icon::Plus)}
            {label}
        </button>
    }
    .into_view()
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
    let draft = create_rw_signal(ServiceForm::from(&service));
    let baseline = create_rw_signal(draft.get_untracked());
    dirty_group(format!("{name}:{}", section.title()), draft, baseline);
    let name = store_value(name);
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
                replicas: service.replicas,
            }),
            Section::Environment => ServiceEdit::Environment(service.environment.clone()),
            Section::Process => ServiceEdit::Process(ServiceProcess {
                command: service.command.clone(),
                arguments: service.arguments.clone(),
            }),
            Section::Storage => ServiceEdit::Mounts(service.mounts.clone()),
            Section::Health => ServiceEdit::Healthcheck(service.healthcheck.clone()),
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
            <fieldset disabled={move || context.blocked() || context.managed()}>
                {service_fields(section, draft)}
                {save_actions(draft, baseline, save, || false)}
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
        Section::Resources => "Leave a limit blank to use the runtime default.",
    }
}

/// Input fields for one service `Section`, bound to the shared draft form.
pub(super) fn service_fields(section: Section, form: RwSignal<ServiceForm>) -> View {
    match section {
        Section::General => {
            let git_source = create_memo(move |_| form.with(|form| form.source_kind == "git"));
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
                        </select>
                    </label>
                    {text_input("Replicas", form, |v| v.replicas.clone(), |v, s| v.replicas = s)}
                    {move || {
                        if git_source.get() {
                            view! {
                                {text_input(
                                    "Repository",
                                    form,
                                    |v| v.repository.clone(),
                                    |v, s| v.repository = s,
                                )}
                                {text_input("Branch", form, |v| v.branch.clone(), |v, s| v.branch = s)}
                                {text_input(
                                    "Commit (optional)",
                                    form,
                                    |v| v.commit.clone(),
                                    |v, s| v.commit = s,
                                )}
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
                            }
                                .into_view()
                        } else {
                            text_input("Container image", form, |v| v.image.clone(), |v, s| v.image = s)
                        }
                    }}
                </div>
            }
            .into_view()
        }
        Section::Environment => environment_fields(form),
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
        .into_view(),
        Section::Storage => mount_fields(form),
        Section::Health => health_fields(form),
        Section::Resources => view! {
            <div class="form-grid">
                {text_input("CPU (millicores)", form, |v| v.cpu.clone(), |v, s| v.cpu = s)}
                {text_input("Memory (bytes)", form, |v| v.memory.clone(), |v, s| v.memory = s)}
            </div>
        }
        .into_view(),
    }
}

/// Editable list of strings (one element per row, no shell parsing) selected
/// from the form by the `read`/`write` accessors.
pub(super) fn string_rows(
    label: &'static str,
    form: RwSignal<ServiceForm>,
    read: fn(&ServiceForm) -> &Vec<String>,
    write: fn(&mut ServiceForm) -> &mut Vec<String>,
) -> View {
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
                            {remove_button(move || form.update(|v| { write(v).remove(i); }))}
                        </div>
                    }
                }}
            />
        </div>
        {add_button(label, move || form.update(|v| write(v).push(String::new())))}
    }
    .into_view()
}
/// Key/value rows for the service environment.
pub(super) fn environment_fields(form: RwSignal<ServiceForm>) -> View {
    view! {
        <div class="form-list">
            <For
                each={move || (0..form.with(|v| v.environment.len())).collect::<Vec<_>>()}
                key={|i| *i}
                children={move |i| {
                    view! {
                        <div class="form-row">
                            {text_input(
                                "Key",
                                form,
                                move |v| {
                                    v.environment.get(i).map_or_else(String::new, |v| v.0.clone())
                                },
                                move |v, s| {
                                    if let Some(value) = v.environment.get_mut(i) {
                                        value.0 = s;
                                    }
                                },
                            )}
                            {text_input(
                                "Value",
                                form,
                                move |v| {
                                    v.environment.get(i).map_or_else(String::new, |v| v.1.clone())
                                },
                                move |v, s| {
                                    if let Some(value) = v.environment.get_mut(i) {
                                        value.1 = s;
                                    }
                                },
                            )}
                            {remove_button(move || form.update(|v| { v.environment.remove(i); }))}
                        </div>
                    }
                }}
            />
        </div>
        {add_button(
            "Add variable",
            move || form.update(|v| v.environment.push((String::new(), String::new()))),
        )}
    }
    .into_view()
}
/// Volume mount rows: volume name, container path and read-only flag.
pub(super) fn mount_fields(form: RwSignal<ServiceForm>) -> View {
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
                                move |v| v.mounts.get(i).map_or_else(String::new, |v| v.volume.clone()),
                                move |v, s| {
                                    if let Some(value) = v.mounts.get_mut(i) {
                                        value.volume = s;
                                    }
                                },
                            )}
                            {text_input(
                                "Container path",
                                form,
                                move |v| v.mounts.get(i).map_or_else(String::new, |v| v.target.clone()),
                                move |v, s| {
                                    if let Some(value) = v.mounts.get_mut(i) {
                                        value.target = s;
                                    }
                                },
                            )}
                            <label class="checkbox">
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
                            {remove_button(move || form.update(|v| { v.mounts.remove(i); }))}
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
    .into_view()
}
/// Health check type selector with the interval, timeout, and HTTP or command
/// fields relevant to the chosen type.
pub(super) fn health_fields(form: RwSignal<ServiceForm>) -> View {
    view! {
        <div class="form-grid">
            <label class="field">
                <span>"Check type"</span>
                <select
                    prop:value={move || form.with(|v| v.health_kind.clone())}
                    on:change={move |event| form.update(|v| v.health_kind = event_target_value(&event))}
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
            <Show when={move || form.with(|v| v.health_kind != "none")}>
                {text_input(
                    "Interval (seconds)",
                    form,
                    |v| v.interval.clone(),
                    |v, s| v.interval = s,
                )}
                {text_input("Timeout (seconds)", form, |v| v.timeout.clone(), |v, s| v.timeout = s)}
            </Show>
            <Show when={move || form.with(|v| v.health_kind == "http")}>
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
    .into_view()
}

/// "Add service" button and modal that saves a new single-replica image service.
#[component]
pub(super) fn NewService() -> impl IntoView {
    let context = editor();
    let opened = create_rw_signal(false);
    let fields = create_rw_signal((String::new(), String::new()));
    let baseline = create_rw_signal(fields.get_untracked());
    dirty_group("new-service".into(), fields, baseline);
    let save = move |()| {
        let (name, image) = fields.get_untracked();
        let service = Service {
            secrets: Vec::new(),
            name,
            source: Source::Image { image },
            replicas: 1,
            environment: BTreeMap::new(),
            command: Vec::new(),
            arguments: Vec::new(),
            mounts: Vec::new(),
            healthcheck: None,
            resources: None,
        };
        context.save(
            ApplicationEdit::AddService(service),
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
                    <button type="submit" class="btn btn-primary" disabled={move || context.blocked()}>
                        "Add service"
                    </button>
                </div>
            </form>
        </Modal>
    }
}
