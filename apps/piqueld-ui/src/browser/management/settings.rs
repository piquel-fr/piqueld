//! Saved configuration forms and service editors.
use super::{Modal, dirty_group, editor, mutation_client, text_input};
use crate::editor::{Section, ServiceForm};
use leptos::{
    Callback, For, IntoView, RwSignal, Show, SignalGet, SignalGetUntracked, SignalSet,
    SignalUpdate, SignalWith, SignalWithUntracked, View, component, create_rw_signal,
    event_target_checked, event_target_value, spawn_local, view,
};
use piqueld_client::{
    ApplicationView, GitRepository, Mount, RepositoryManifest, Service, Source, Volume,
};
use std::collections::BTreeMap;

#[component]
pub(super) fn RepositorySettings() -> impl IntoView {
    let context = editor();
    let backing = context.saved.get_untracked().application.spec.manifest;
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
    let save = move |_| {
        if other_edits() {
            return;
        }
        let value = draft.get_untracked();
        let mut manifest = context.manifest();
        manifest.spec.manifest = value.0.then_some(value.1);
        context.save(
            manifest,
            Callback::new(move |_| baseline.set(draft.get_untracked())),
        );
    };
    view! {
        <section class="settings-card">
            <h3>"Repository manifest"</h3>
            <Show when={other_edits}>
                <p class="help">
                    "Save or discard edits in other settings before changing the repository connection."
                </p>
            </Show>
            <fieldset disabled={move || context.blocked()}>
                <label>
                    <input
                        type="checkbox"
                        prop:checked={move || draft.get().0}
                        on:change={move |ev| draft.update(|v| v.0 = event_target_checked(&ev))}
                    />
                    "Load configuration from Git on Deploy"
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
                <div class="form-actions" hidden={move || draft.get() == baseline.get()}>
                    <button
                        class="primary"
                        disabled={move || draft.get() == baseline.get() || other_edits()}
                        on:click={save}
                    >
                        "Save changes"
                    </button>
                    <button on:click={move |_| {
                        draft.set(baseline.get_untracked());
                    }}>"Discard edits"</button>
                </div>
            </fieldset>
        </section>
    }
}

#[component]
pub(super) fn MetadataSettings() -> impl IntoView {
    let context = editor();
    let editing = create_rw_signal(false);
    let name = create_rw_signal(
        context
            .saved
            .with_untracked(|a| a.application.metadata.name.clone()),
    );
    let baseline = create_rw_signal(name.get_untracked());
    dirty_group("name".into(), name, baseline);
    let save = move |()| {
        if context.blocked() {
            return;
        }
        let Ok(client) = mutation_client() else {
            context
                .error
                .set(Some("Unable to create request identity.".into()));
            return;
        };
        let app = context.saved.get_untracked();
        let request = piqueld_client::RenameApplicationRequest {
            name: name.get_untracked(),
            expected_generation: Some(app.generation),
        };
        context.busy.set(true);
        context.error.set(None);
        spawn_local(async move {
            let result = client
                .rename_application(app.application.id.as_str(), &request)
                .await;
            match result {
                Ok(renamed) => {
                    context.saved.update(|app| {
                        app.application.metadata.name.clone_from(&renamed.name);
                        app.generation = renamed.generation;
                    });
                    name.set(renamed.name.clone());
                    baseline.set(renamed.name);
                    editing.set(false);
                    context.notice.set("Application name saved.".into());
                    context.dashboard.with_value(|d| (d.refresh)());
                }
                Err(error) => context.failure(&error),
            }
            context.busy.set(false);
        });
    };
    view! {
        <div class="application-name">
            <div class="name-display" hidden={move || editing.get()}>
                <h1>{move || context.saved.with(|a| a.application.metadata.name.clone())}</h1>
                <button
                    class="icon-button"
                    aria-label="Rename application"
                    title="Rename application"
                    disabled={move || context.blocked()}
                    on:click={move |_| editing.set(true)}
                >
                    <svg
                        width="16"
                        height="16"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.5"
                        aria-hidden="true"
                    >
                        <path d="m16 3 5 5-12 12-6 1 1-6Z M14 5l5 5" />
                    </svg>
                </button>
            </div>
            <form
                hidden={move || !editing.get()}
                on:submit={move |event| {
                    event.prevent_default();
                    save(());
                }}
            >
                <fieldset class="rename-form" disabled={move || context.blocked()}>
                    {text_input("Application name", name, String::clone, |v, s| *v = s)}
                    <button
                        type="submit"
                        class="primary"
                        disabled={move || name.get() == baseline.get()}
                    >
                        "Save"
                    </button>
                    <button
                        type="button"
                        on:click={move |_| {
                            name.set(baseline.get_untracked());
                            editing.set(false);
                        }}
                    >
                        "Cancel"
                    </button>
                </fieldset>
            </form>
        </div>
    }
}

#[component]
pub(super) fn VolumeSettings() -> impl IntoView {
    let context = editor();
    let volumes = create_rw_signal(context.saved.with_untracked(|a| {
        a.application
            .spec
            .volumes
            .iter()
            .map(|v| v.name.clone())
            .collect::<Vec<_>>()
    }));
    let baseline = create_rw_signal(volumes.get_untracked());
    dirty_group("volumes".into(), volumes, baseline);
    let save = move |_| {
        let mut manifest = context.manifest();
        manifest.spec.volumes = volumes
            .get_untracked()
            .into_iter()
            .map(|name| Volume { name })
            .collect();
        context.save(
            manifest,
            Callback::new(move |app: ApplicationView| {
                let names = app
                    .application
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
        <section class="settings-card">
            <h3>"Named volumes"</h3>
            <fieldset disabled={move || context.blocked()}>
                <For
                    each={move || (0..volumes.with(Vec::len)).collect::<Vec<_>>()}
                    key={|i| *i}
                    children={move |i| {
                        view! {
                            <div class="field-row">
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
                                <button on:click={move |_| {
                                    volumes
                                        .update(|v| {
                                            v.remove(i);
                                        });
                                }}>"Remove"</button>
                            </div>
                        }
                    }}
                />
                <button on:click={move |_| {
                    volumes.update(|v| v.push(String::new()));
                }}>"+ Add volume"</button>
                <div class="form-actions" hidden={move || volumes.get() == baseline.get()}>
                    <button
                        class="primary"
                        disabled={move || volumes.get() == baseline.get()}
                        on:click={save}
                    >
                        "Save changes"
                    </button>
                    <button on:click={move |_| {
                        volumes.set(baseline.get_untracked());
                    }}>"Discard edits"</button>
                </div>
            </fieldset>
        </section>
    }
}

#[component]
pub(super) fn ServiceGroup(name: String, section: Section) -> impl IntoView {
    let context = editor();
    let service = context
        .saved
        .with_untracked(|a| {
            a.application
                .spec
                .services
                .iter()
                .find(|s| s.name == name)
                .cloned()
        })
        .expect("listed service");
    let draft = create_rw_signal(ServiceForm::from(&service));
    let baseline = create_rw_signal(draft.get_untracked());
    dirty_group(format!("{name}:{}", section.title()), draft, baseline);
    let save = move |_| {
        let mut manifest = context.manifest();
        let Some(service) = manifest.spec.services.iter_mut().find(|s| s.name == name) else {
            context
                .error
                .set(Some("Service was removed. Reload configuration.".into()));
            return;
        };
        if let Err(error) = draft.get_untracked().patch(section, service) {
            context.error.set(Some(error));
            return;
        }
        let sent = draft.get_untracked();
        context.save(manifest, Callback::new(move |_| baseline.set(sent.clone())));
    };
    view! {
        <section class="settings-card service-settings">
            <fieldset disabled={move || {
                context.blocked() || context.saved.get().application.spec.manifest.is_some()
            }}>
                {service_fields(section, draft)}
                <div class="form-actions" hidden={move || draft.get() == baseline.get()}>
                    <button
                        class="primary"
                        disabled={move || draft.get() == baseline.get()}
                        on:click={save}
                    >
                        "Save changes"
                    </button>
                    <button on:click={move |_| {
                        draft.set(baseline.get_untracked());
                    }}>"Discard edits"</button>
                </div>
            </fieldset>
        </section>
    }
}

pub(super) fn service_fields(section: Section, form: RwSignal<ServiceForm>) -> View {
    match section {
        Section::General => view! {
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
                        <option value="git">"Git / Dockerfile"</option>
                    </select>
                </label>
                {move || {
                    if form.get().source_kind == "git" {
                        view! {
                            <>
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
                            </>
                        }
                            .into_view()
                    } else {
                        text_input("Container image", form, |v| v.image.clone(), |v, s| v.image = s)
                    }
                }}
                {text_input("Replicas", form, |v| v.replicas.clone(), |v, s| v.replicas = s)}
            </div>
        }
        .into_view(),
        Section::Environment => environment_fields(form),
        Section::Process => view! {
            <p class="help">
                "Each row is one element. Spaces are preserved; no shell parsing is applied."
            </p>
            {string_rows("Command element", form, |v| &v.command, |v| &mut v.command)}
            {string_rows("Argument", form, |v| &v.arguments, |v| &mut v.arguments)}
        }
        .into_view(),
        Section::Storage => mount_fields(form),
        Section::Health => health_fields(form),
        Section::Resources => view! {
            <p class="help">"Leave a limit blank to use the runtime default."</p>
            <div class="form-grid">
                {text_input("CPU (millicores)", form, |v| v.cpu.clone(), |v, s| v.cpu = s)}
                {text_input("Memory (bytes)", form, |v| v.memory.clone(), |v, s| v.memory = s)}
            </div>
        }
        .into_view(),
    }
}

pub(super) fn string_rows(
    label: &'static str,
    form: RwSignal<ServiceForm>,
    read: fn(&ServiceForm) -> &Vec<String>,
    write: fn(&mut ServiceForm) -> &mut Vec<String>,
) -> View {
    view! {
        <div class="collection">
            <For
                each={move || (0..form.with(|v| read(v).len())).collect::<Vec<_>>()}
                key={|i| *i}
                children={move |i| {
                    view! {
                        <div class="field-row">
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
                            <button on:click={move |_| {
                                form.update(|v| {
                                    write(v).remove(i);
                                });
                            }}>"Remove"</button>
                        </div>
                    }
                }}
            />
            <button on:click={move |_| {
                form.update(|v| write(v).push(String::new()));
            }}>{format!("+ Add {}", label.to_lowercase())}</button>
        </div>
    }
    .into_view()
}
pub(super) fn environment_fields(form: RwSignal<ServiceForm>) -> View {
    view! {
        <div class="collection">
            <For
                each={move || (0..form.with(|v| v.environment.len())).collect::<Vec<_>>()}
                key={|i| *i}
                children={move |i| {
                    view! {
                        <div class="field-row">
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
                            <button on:click={move |_| {
                                form.update(|v| {
                                    v.environment.remove(i);
                                });
                            }}>"Remove"</button>
                        </div>
                    }
                }}
            />
            <button on:click={move |_| {
                form.update(|v| v.environment.push((String::new(), String::new())));
            }}>"+ Add variable"</button>
        </div>
    }
    .into_view()
}
pub(super) fn mount_fields(form: RwSignal<ServiceForm>) -> View {
    view! {
        <p class="help">"Use a declared volume name and an absolute container path."</p>
        <For
            each={move || (0..form.with(|v| v.mounts.len())).collect::<Vec<_>>()}
            key={|i| *i}
            children={move |i| {
                view! {
                    <div class="field-row">
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
                        )}<label class="checkbox">
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
                        <button on:click={move |_| {
                            form.update(|v| {
                                v.mounts.remove(i);
                            });
                        }}>"Remove"</button>
                    </div>
                }
            }}
        />
        <button on:click={move |_| {
            form.update(|v| {
                v.mounts
                    .push(Mount {
                        volume: String::new(),
                        target: String::new(),
                        read_only: false,
                    });
            });
        }}>"+ Add mount"</button>
    }
    .into_view()
}
pub(super) fn health_fields(form: RwSignal<ServiceForm>) -> View {
    view! {
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
            <div class="form-grid">
                {text_input(
                    "Interval (seconds)",
                    form,
                    |v| v.interval.clone(),
                    |v, s| v.interval = s,
                )}
                {text_input("Timeout (seconds)", form, |v| v.timeout.clone(), |v, s| v.timeout = s)}
            </div>
            <Show when={move || form.with(|v| v.health_kind == "http")}>
                <div class="form-grid">
                    {text_input("Port", form, |v| v.port.clone(), |v, s| v.port = s)}
                    {text_input("Path", form, |v| v.path.clone(), |v, s| v.path = s)}
                </div>
            </Show>
            <Show when={move || {
                form.with(|v| v.health_kind == "command")
            }}>
                {string_rows(
                    "Health command element",
                    form,
                    |v| &v.health_command,
                    |v| &mut v.health_command,
                )}
            </Show>
        </Show>
    }
    .into_view()
}

#[component]
pub(super) fn NewService() -> impl IntoView {
    let context = editor();
    let opened = create_rw_signal(false);
    let fields = create_rw_signal((String::new(), String::new()));
    let baseline = create_rw_signal(fields.get_untracked());
    dirty_group("new-service".into(), fields, baseline);
    let save = move |()| {
        let (name, image) = fields.get_untracked();
        let mut manifest = context.manifest();
        manifest.spec.services.push(Service {
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
        });
        context.save(
            manifest,
            Callback::new(move |_| {
                fields.set((String::new(), String::new()));
                opened.set(false);
            }),
        );
    };
    view! {
        <button disabled={move || context.blocked()} on:click={move |_| opened.set(true)}>
            "Add Service"
        </button>
        <Modal
            title="Add service"
            opened={opened}
            busy={context.busy}
            on_close={Callback::new(move |()| {
                fields.set(baseline.get_untracked());
                context.error.set(None);
            })}
        >
            <form on:submit={move |event| {
                event.prevent_default();
                save(());
            }}>
                <fieldset disabled={move || {
                    context.blocked()
                }}>
                    {text_input("Service name", fields, |v| v.0.clone(), |v, s| v.0 = s)}
                    {text_input("Container image", fields, |v| v.1.clone(), |v, s| v.1 = s)}
                    <div class="form-actions">
                        <button type="submit" class="primary">
                            "Add service"
                        </button>
                    </div>
                </fieldset>
                {move || {
                    context
                        .error
                        .get()
                        .map(|error| {
                            view! {
                                <p class="form-error" role="alert">
                                    {error}
                                </p>
                            }
                        })
                }}
            </form>
        </Modal>
    }
}
