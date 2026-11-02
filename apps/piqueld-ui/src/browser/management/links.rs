//! Private links: the services this application lets others reach, and the
//! services of other applications it reaches.
use super::super::ui::{add_button, empty, remove_button};
use super::{Draft, Rows, editor};
use crate::application::{ExportDraft, ImportDraft};
use leptos::prelude::*;
use piqueld_client::edit::ApplicationEdit;

/// A text input bound to one field of row `index`.
fn row_field<D: Send + Sync + 'static>(
    rows: RwSignal<Vec<D>>,
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
                prop:value={move || rows.field(index, read)}
                on:input={move |event| {
                    rows.edit(index, |row| write(row, event_target_value(&event)));
                }}
            />
        </label>
    }
}

/// Export editor: which services other applications may reach, and which
/// applications may. Saving replaces every export.
#[component]
fn ExportSettings() -> impl IntoView {
    let context = editor();
    let draft = Draft::synced("exports", ExportDraft::rows);
    let exports = draft.value;
    let save = move || {
        let saved = exports
            .get_untracked()
            .into_iter()
            .map(ExportDraft::export)
            .collect();
        draft.save(ApplicationEdit::Exports(saved), ExportDraft::rows);
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
                <Show when={move || {
                    exports.with(Vec::is_empty)
                }}>
                    {empty(
                        "No exports. Other applications cannot reach this application's services.",
                    )}
                </Show>
                <For
                    each={move || exports.indices()}
                    key={|index| *index}
                    children={move |index| {
                        view! {
                            <div class="form-row" data-export={index}>
                                <label class="field">
                                    <span>"Service"</span>
                                    <select
                                        prop:value={move || {
                                            exports.field(index, |e| e.service.clone())
                                        }}
                                        on:change={move |event| {
                                            exports
                                                .edit(index, |e| e.service = event_target_value(&event));
                                        }}
                                    >
                                        {context.service_options()}
                                    </select>
                                </label>
                                {row_field(
                                    exports,
                                    index,
                                    "Applications",
                                    "notes, billing",
                                    |e| e.applications.clone(),
                                    |e, value| e.applications = value,
                                )}
                                <label class="checkbox">
                                    <input
                                        type="checkbox"
                                        prop:checked={move || exports.field(index, |e| e.previews)}
                                        on:change={move |event| {
                                            exports
                                                .edit(index, |e| e.previews = event_target_checked(&event));
                                        }}
                                    />
                                    <span>"Previews too"</span>
                                </label>
                                {remove_button(move || exports.remove(index))}
                            </div>
                        }
                    }}
                />
                <div>
                    {add_button(
                        "Add export",
                        move || exports.update(|exports| exports.push(ExportDraft::default())),
                    )}
                </div>
                {draft.actions(save, || false)}
            </fieldset>
        </section>
    }
}

/// Import editor: services of other applications this one reaches. Saving
/// replaces every import.
#[component]
fn ImportSettings() -> impl IntoView {
    let context = editor();
    let draft = Draft::synced("imports", ImportDraft::rows);
    let imports = draft.value;
    let save = move || {
        let saved = imports
            .get_untracked()
            .into_iter()
            .map(ImportDraft::import)
            .collect();
        draft.save(ApplicationEdit::Imports(saved), ImportDraft::rows);
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
                <Show when={move || imports.with(Vec::is_empty)}>{empty("No imports.")}</Show>
                <For
                    each={move || imports.indices()}
                    key={|index| *index}
                    children={move |index| {
                        view! {
                            <div class="form-row" data-import={index}>
                                {row_field(
                                    imports,
                                    index,
                                    "Application",
                                    "auth",
                                    |i| i.application.clone(),
                                    |i, value| i.application = value,
                                )}
                                {row_field(
                                    imports,
                                    index,
                                    "Service",
                                    "api",
                                    |i| i.service.clone(),
                                    |i, value| i.service = value,
                                )}
                                {row_field(
                                    imports,
                                    index,
                                    "Environment",
                                    "Same name",
                                    |i| i.environment.clone(),
                                    |i, value| i.environment = value,
                                )}
                                {remove_button(move || imports.remove(index))}
                            </div>
                        }
                    }}
                />
                <div>
                    {add_button(
                        "Add import",
                        move || imports.update(|imports| imports.push(ImportDraft::default())),
                    )}
                </div>
                {draft.actions(save, || false)}
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
