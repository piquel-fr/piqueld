//! Private links: the services this application lets others reach, and the
//! services of other applications it reaches.
use super::super::ui::{Icon, empty, icon, remove_button};
use super::{dirty_group, editor, save_actions};
use leptos::prelude::*;
use piqueld_client::{Export, Import, edit::ApplicationEdit};

/// One editable export. Applications are typed as one comma-separated list.
#[derive(Clone, Default, PartialEq)]
struct ExportDraft {
    service: String,
    applications: String,
    previews: bool,
}

impl ExportDraft {
    /// Saved exports as editable rows.
    fn rows(exports: Vec<Export>) -> Vec<Self> {
        exports
            .into_iter()
            .map(|export| Self {
                service: export.service,
                applications: export.applications.join(", "),
                previews: export.previews,
            })
            .collect()
    }

    /// The export input; validation reports empty or invalid names.
    fn export(self) -> Export {
        Export {
            service: self.service,
            applications: self
                .applications
                .split(',')
                .map(str::trim)
                .filter(|application| !application.is_empty())
                .map(str::to_owned)
                .collect(),
            previews: self.previews,
        }
    }
}

/// One editable import. An empty environment reaches the one named like the
/// importing environment.
#[derive(Clone, Default, PartialEq)]
struct ImportDraft {
    application: String,
    service: String,
    environment: String,
}

impl ImportDraft {
    /// Saved imports as editable rows.
    fn rows(imports: Vec<Import>) -> Vec<Self> {
        imports
            .into_iter()
            .map(|import| Self {
                application: import.application,
                service: import.service,
                environment: import
                    .environment
                    .map(|environment| environment.to_string())
                    .unwrap_or_default(),
            })
            .collect()
    }

    /// The import input.
    fn import(self) -> Import {
        Import {
            application: self.application,
            service: self.service,
            environment: Some(self.environment)
                .filter(|environment| !environment.is_empty())
                .map(Into::into),
        }
    }
}

/// Reads one field of row `index`, empty when the row was removed.
fn field<D, T: Default>(draft: RwSignal<Vec<D>>, index: usize, read: impl Fn(&D) -> T) -> T
where
    D: Send + Sync + 'static,
{
    draft.with(|rows| rows.get(index).map(read).unwrap_or_default())
}

/// Updates row `index` in place.
fn edit<D>(draft: RwSignal<Vec<D>>, index: usize, write: impl FnOnce(&mut D))
where
    D: Send + Sync + 'static,
{
    draft.update(|rows| {
        if let Some(row) = rows.get_mut(index) {
            write(row);
        }
    });
}

/// A text input bound to one field of row `index`.
fn text_field<D: Send + Sync + 'static>(
    draft: RwSignal<Vec<D>>,
    index: usize,
    label: &'static str,
    placeholder: &'static str,
    read: fn(&D) -> String,
    write: fn(&mut D, String),
) -> impl IntoView {
    view! {
        <label class="field">
            <span>{label}</span>
            <input
                type="text"
                placeholder={placeholder}
                prop:value={move || field(draft, index, read)}
                on:input={move |event| edit(draft, index, |row| write(row, event_target_value(&event)))}
            />
        </label>
    }
}

/// Export editor: which services other applications may reach, and which
/// applications may. Saving replaces every export.
#[component]
fn ExportSettings() -> impl IntoView {
    let context = editor();
    let draft = RwSignal::new(ExportDraft::rows(context.manifest().spec.exports));
    let baseline = RwSignal::new(draft.get_untracked());
    dirty_group("exports".into(), draft, baseline);
    Effect::new(move |_| {
        let saved = context
            .saved
            .with(|saved| ExportDraft::rows(saved.application.to_manifest().spec.exports));
        if draft.get_untracked() == baseline.get_untracked() {
            draft.set(saved.clone());
            baseline.set(saved);
        }
    });
    let save = move || {
        let exports = draft
            .get_untracked()
            .into_iter()
            .map(ExportDraft::export)
            .collect();
        context.save(
            ApplicationEdit::Exports(exports),
            Callback::new(move |saved: piqueld_client::ApplicationView| {
                draft.set(ExportDraft::rows(
                    saved.application.to_manifest().spec.exports,
                ));
                baseline.set(draft.get_untracked());
            }),
        );
    };
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>"Exports"</h3>
                    <p>
                        "Services other applications may reach, without a route. An importing application's services reach an exported service as service.application, on a network only the two join: no other service of this application, and no other importer. Each environment exports to the same-named environment of the listed applications, or to the one they name; previews only when allowed. Any port the service listens on is reachable."
                    </p>
                </div>
            </header>
            <fieldset class="stack-sm" disabled={move || context.blocked()}>
                <Show when={move || draft.with(Vec::is_empty)}>
                    {empty("No exports. Other applications cannot reach this application's services.")}
                </Show>
                <For
                    each={move || (0..draft.with(Vec::len)).collect::<Vec<_>>()}
                    key={|index| *index}
                    children={move |index| {
                        view! {
                            <div class="form-row" data-export={index}>
                                <label class="field">
                                    <span>"Service"</span>
                                    <select
                                        prop:value={move || field(draft, index, |e: &ExportDraft| e.service.clone())}
                                        on:change={move |event| {
                                            edit(draft, index, |e| e.service = event_target_value(&event));
                                        }}
                                    >
                                        <option value="">"Select a service"</option>
                                        {move || {
                                            context
                                                .manifest()
                                                .spec
                                                .services
                                                .into_iter()
                                                .map(|service| {
                                                    view! {
                                                        <option value={service
                                                            .name
                                                            .clone()}>{service.name.clone()}</option>
                                                    }
                                                })
                                                .collect_view()
                                        }}
                                    </select>
                                </label>
                                {text_field(
                                    draft,
                                    index,
                                    "Applications",
                                    "notes, billing",
                                    |e| e.applications.clone(),
                                    |e, value| e.applications = value,
                                )}
                                <label class="checkbox">
                                    <input
                                        type="checkbox"
                                        prop:checked={move || field(draft, index, |e: &ExportDraft| e.previews)}
                                        on:change={move |event| {
                                            edit(draft, index, |e| e.previews = event_target_checked(&event));
                                        }}
                                    />
                                    <span>"Previews too"</span>
                                </label>
                                {remove_button(move || {
                                    draft
                                        .update(|exports| {
                                            exports.remove(index);
                                        });
                                })}
                            </div>
                        }
                    }}
                />
                <div>
                    <button
                        type="button"
                        class="btn btn-sm"
                        on:click={move |_| draft.update(|exports| exports.push(ExportDraft::default()))}
                    >
                        {icon(Icon::Plus)}
                        "Add export"
                    </button>
                </div>
                {save_actions(draft, baseline, save, || false)}
            </fieldset>
        </section>
    }
}

/// Import editor: services of other applications this one reaches. Saving
/// replaces every import.
#[component]
fn ImportSettings() -> impl IntoView {
    let context = editor();
    let draft = RwSignal::new(ImportDraft::rows(context.manifest().spec.imports));
    let baseline = RwSignal::new(draft.get_untracked());
    dirty_group("imports".into(), draft, baseline);
    Effect::new(move |_| {
        let saved = context
            .saved
            .with(|saved| ImportDraft::rows(saved.application.to_manifest().spec.imports));
        if draft.get_untracked() == baseline.get_untracked() {
            draft.set(saved.clone());
            baseline.set(saved);
        }
    });
    let save = move || {
        let imports = draft
            .get_untracked()
            .into_iter()
            .map(ImportDraft::import)
            .collect();
        context.save(
            ApplicationEdit::Imports(imports),
            Callback::new(move |saved: piqueld_client::ApplicationView| {
                draft.set(ImportDraft::rows(
                    saved.application.to_manifest().spec.imports,
                ));
                baseline.set(draft.get_untracked());
            }),
        );
    };
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>"Imports"</h3>
                    <p>
                        "Services of other applications this application's services reach, as service.application, once that application exports them to it. Leave the environment empty to reach the one named like each environment of this application; previews must name one, for example with a variable. Each environment's page shows whether its imports connect."
                    </p>
                </div>
            </header>
            <fieldset class="stack-sm" disabled={move || context.blocked()}>
                <Show when={move || draft.with(Vec::is_empty)}>
                    {empty("No imports.")}
                </Show>
                <For
                    each={move || (0..draft.with(Vec::len)).collect::<Vec<_>>()}
                    key={|index| *index}
                    children={move |index| {
                        view! {
                            <div class="form-row" data-import={index}>
                                {text_field(
                                    draft,
                                    index,
                                    "Application",
                                    "auth",
                                    |i| i.application.clone(),
                                    |i, value| i.application = value,
                                )}
                                {text_field(
                                    draft,
                                    index,
                                    "Service",
                                    "api",
                                    |i| i.service.clone(),
                                    |i, value| i.service = value,
                                )}
                                {text_field(
                                    draft,
                                    index,
                                    "Environment",
                                    "Same name",
                                    |i| i.environment.clone(),
                                    |i, value| i.environment = value,
                                )}
                                {remove_button(move || {
                                    draft
                                        .update(|imports| {
                                            imports.remove(index);
                                        });
                                })}
                            </div>
                        }
                    }}
                />
                <div>
                    <button
                        type="button"
                        class="btn btn-sm"
                        on:click={move |_| draft.update(|imports| imports.push(ImportDraft::default()))}
                    >
                        {icon(Icon::Plus)}
                        "Add import"
                    </button>
                </div>
                {save_actions(draft, baseline, save, || false)}
            </fieldset>
        </section>
    }
}

/// The Links tab: exports, then imports.
#[component]
pub(super) fn LinkSettings() -> impl IntoView {
    view! {
        <div class="stack">
            <ExportSettings />
            <ImportSettings />
        </div>
    }
}
