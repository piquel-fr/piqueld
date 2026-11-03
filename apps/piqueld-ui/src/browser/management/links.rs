//! Private links: the services this application lets others reach, and the
//! services of other applications it reaches. Each is a row, added and
//! edited in a dialog; saving replaces the whole list.
use super::super::confirm;
use super::super::ui::{FormModal, Icon, empty, icon, row_actions, text_input};
use super::{RowDialog, editor};
use crate::application::{ExportDraft, ImportDraft};
use leptos::prelude::*;
use piqueld_client::edit::ApplicationEdit;

/// Export editor: which services other applications may reach, and which
/// applications may.
#[component]
fn ExportSettings() -> impl IntoView {
    let context = editor();
    let dialog = RowDialog::<ExportDraft>::new();
    let draft = dialog.draft;
    // Saves `rows` as the application's exports, closing the dialog.
    let save = move |rows: Vec<ExportDraft>| {
        let exports = rows.into_iter().map(ExportDraft::export).collect();
        context.save(ApplicationEdit::Exports(exports), move |_| {
            dialog.opened.set(false);
        });
    };
    let remove = move |index: usize| {
        if !confirm(
            "Stop exporting this service? Importing applications lose it at their next deployment.",
        ) {
            return;
        }
        let mut rows = ExportDraft::rows(&context.manifest());
        if index < rows.len() {
            rows.remove(index);
        }
        save(rows);
    };
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>"Exports"</h3>
                    <p>
                        "Services other applications may reach, without a route, as service.application on a network only the two share. Each environment exports to the same-named environment of the listed applications; previews only when allowed."
                    </p>
                </div>
                <button
                    type="button"
                    class="btn btn-sm"
                    disabled={move || context.blocked()}
                    on:click={move |_| dialog.add()}
                >
                    {icon(Icon::Plus)}
                    "Add export"
                </button>
            </header>
            {move || {
                let rows = ExportDraft::rows(&context.manifest());
                if rows.is_empty() {
                    return empty(
                        "No exports. Other applications cannot reach this application's services.",
                    );
                }
                view! {
                    <ul class="list" aria-label="Exports">
                        {rows
                            .into_iter()
                            .enumerate()
                            .map(|(index, row)| {
                                let edited = row.clone();
                                let reach = format!(
                                    "to {}{}",
                                    if row.applications.is_empty() {
                                        "no application".to_owned()
                                    } else {
                                        row.applications.clone()
                                    },
                                    if row.previews { ", and their previews" } else { "" },
                                );
                                view! {
                                    <li class="list-row">
                                        <span class="title">
                                            <code>{row.service.clone()}</code>
                                            <small>{reach}</small>
                                        </span>
                                        {row_actions(
                                            move || dialog.edit(index, edited.clone()),
                                            move || remove(index),
                                        )}
                                    </li>
                                }
                            })
                            .collect_view()}
                    </ul>
                }
                    .into_any()
            }}
            <FormModal
                title={dialog.title("Add export", "Edit export")}
                opened={dialog.opened}
                busy={context.busy}
                error={context.error}
                submit={dialog.title("Add export", "Save export")}
                on_submit={Callback::new(move |()| {
                    save(dialog.apply(ExportDraft::rows(&context.manifest())));
                })}
            >
                <label class="field">
                    <span>"Service"</span>
                    <select
                        prop:value={move || draft.with(|e| e.service.clone())}
                        on:change={move |event| {
                            draft.update(|e| e.service = event_target_value(&event));
                        }}
                    >
                        {context.service_options()}
                    </select>
                </label>
                {text_input(
                    "Applications",
                    draft,
                    |e| e.applications.clone(),
                    |e, value| e.applications = value,
                )}
                <p class="hint">"Application names, separated by commas."</p>
                <label class="checkbox">
                    <input
                        type="checkbox"
                        prop:checked={move || draft.with(|e| e.previews)}
                        on:change={move |event| {
                            draft.update(|e| e.previews = event_target_checked(&event));
                        }}
                    />
                    "Previews too"
                </label>
            </FormModal>
        </section>
    }
}

/// Import editor: services of other applications this one reaches.
#[component]
fn ImportSettings() -> impl IntoView {
    let context = editor();
    let dialog = RowDialog::<ImportDraft>::new();
    let draft = dialog.draft;
    // Saves `rows` as the application's imports, closing the dialog.
    let save = move |rows: Vec<ImportDraft>| {
        let imports = rows.into_iter().map(ImportDraft::import).collect();
        context.save(ApplicationEdit::Imports(imports), move |_| {
            dialog.opened.set(false);
        });
    };
    let remove = move |index: usize| {
        if !confirm("Stop importing this service? Services lose it at the next deployment.") {
            return;
        }
        let mut rows = ImportDraft::rows(&context.manifest());
        if index < rows.len() {
            rows.remove(index);
        }
        save(rows);
    };
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>"Imports"</h3>
                    <p>
                        "Services of other applications this one reaches, as service.application, once exported to it. Without an environment, each environment reaches its namesake; previews must name one, for example with a variable. Each environment's page shows whether its imports connect."
                    </p>
                </div>
                <button
                    type="button"
                    class="btn btn-sm"
                    disabled={move || context.blocked()}
                    on:click={move |_| dialog.add()}
                >
                    {icon(Icon::Plus)}
                    "Add import"
                </button>
            </header>
            {move || {
                let rows = ImportDraft::rows(&context.manifest());
                if rows.is_empty() {
                    return empty(
                        "No imports. This application reaches no other application's services.",
                    );
                }
                view! {
                    <ul class="list" aria-label="Imports">
                        {rows
                            .into_iter()
                            .enumerate()
                            .map(|(index, row)| {
                                let edited = row.clone();
                                let environment = if row.environment.is_empty() {
                                    "from the same-named environment".to_owned()
                                } else {
                                    format!("from environment {}", row.environment)
                                };
                                view! {
                                    <li class="list-row">
                                        <span class="title">
                                            <code>
                                                {format!("{}.{}", row.service, row.application)}
                                            </code>
                                            <small>{environment}</small>
                                        </span>
                                        {row_actions(
                                            move || dialog.edit(index, edited.clone()),
                                            move || remove(index),
                                        )}
                                    </li>
                                }
                            })
                            .collect_view()}
                    </ul>
                }
                    .into_any()
            }}
            <FormModal
                title={dialog.title("Add import", "Edit import")}
                opened={dialog.opened}
                busy={context.busy}
                error={context.error}
                submit={dialog.title("Add import", "Save import")}
                on_submit={Callback::new(move |()| {
                    save(dialog.apply(ImportDraft::rows(&context.manifest())));
                })}
            >
                {text_input(
                    "Application",
                    draft,
                    |i| i.application.clone(),
                    |i, value| i.application = value,
                )}
                {text_input("Service", draft, |i| i.service.clone(), |i, value| i.service = value)}
                {text_input(
                    "Environment",
                    draft,
                    |i| i.environment.clone(),
                    |i, value| i.environment = value,
                )}
                <p class="hint">
                    "Leave the environment empty to reach the one named like each of this application's environments."
                </p>
            </FormModal>
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
