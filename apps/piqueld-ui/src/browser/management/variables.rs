//! Manifest variables: defaults and per-environment values on the application
//! page, and the values one environment renders with on its page.
use super::super::ui::{Tone, add_button, badge, empty, remove_button};
use super::{Draft, Rows, editor};
use leptos::prelude::*;
use piqueld_client::{
    ApplicationManifest, EnvironmentSource, Variable,
    edit::{ApplicationEdit, Variables},
};
use std::collections::{BTreeMap, BTreeSet};

/// One variable row: its default, its value per environment name, and its
/// value in previews, as typed.
#[derive(Clone, Debug, Default, PartialEq)]
struct VariableDraft {
    name: String,
    default: String,
    environments: BTreeMap<String, String>,
    previews: String,
}

impl VariableDraft {
    /// The manifest's variables as rows.
    fn saved(manifest: &ApplicationManifest) -> Vec<Self> {
        Self::rows(&Variables::of(manifest))
    }

    /// Saved variables as rows, in name order.
    fn rows(variables: &Variables) -> Vec<Self> {
        let names = variables
            .defaults
            .keys()
            .chain(variables.environments.values().flat_map(BTreeMap::keys))
            .chain(variables.previews.keys())
            .collect::<BTreeSet<_>>();
        names
            .into_iter()
            .map(|name| Self {
                name: name.clone(),
                default: variables
                    .defaults
                    .get(name)
                    .map(Variable::to_text)
                    .unwrap_or_default(),
                environments: variables
                    .environments
                    .iter()
                    .filter_map(|(environment, values)| {
                        Some((environment.clone(), values.get(name)?.to_text()))
                    })
                    .collect(),
                previews: variables
                    .previews
                    .get(name)
                    .map(Variable::to_text)
                    .unwrap_or_default(),
            })
            .collect()
    }

    /// The rows as variables; empty cells have no value. Rejects unnamed and
    /// duplicate variables.
    fn variables(rows: &[Self]) -> Result<Variables, String> {
        let mut variables = Variables::default();
        let mut names = BTreeSet::new();
        for row in rows {
            let name = row.name.trim();
            if name.is_empty() {
                return Err("Every variable needs a name.".into());
            }
            if !names.insert(name) {
                return Err(format!("Variable {name:?} appears more than once."));
            }
            if !row.default.trim().is_empty() {
                variables
                    .defaults
                    .insert(name.into(), Variable::from_text(&row.default));
            }
            for (environment, value) in &row.environments {
                if !value.trim().is_empty() {
                    variables
                        .environments
                        .entry(environment.clone())
                        .or_default()
                        .insert(name.into(), Variable::from_text(value));
                }
            }
            if !row.previews.trim().is_empty() {
                variables
                    .previews
                    .insert(name.into(), Variable::from_text(&row.previews));
            }
        }
        Ok(variables)
    }
}

/// Variable editor: one row per variable, with its default, one column per
/// environment, including environments the manifest configures before they
/// exist, and one for previews. Saving replaces every value.
#[component]
pub(super) fn VariableSettings() -> impl IntoView {
    let context = editor();
    let draft = Draft::synced("variables", VariableDraft::saved);
    let rows = draft.value;
    let columns = Memo::new(move |_| {
        let mut names = context.saved.with(|saved| {
            saved
                .environments
                .iter()
                .map(|environment| environment.name.to_string())
                .collect::<BTreeSet<_>>()
        });
        names.extend(Variables::of(&context.manifest()).environments.into_keys());
        names.into_iter().collect::<Vec<_>>()
    });
    let save = move || match VariableDraft::variables(&rows.get_untracked()) {
        Ok(variables) => draft.save(ApplicationEdit::Variables(variables), VariableDraft::saved),
        Err(message) => context.error.set(Some(message)),
    };
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>"Variables"</h3>
                    <p>
                        "Reference a variable as " <code>"${{ vars.<name> }}"</code>
                        " in hostnames, environment values, commands, replicas, limits, and other settings. Each environment uses its own value, else the default; previews use the Previews value, else the default. "
                        <code>"true"</code>", "<code>"false"</code>
                        " and integers keep their type; wrap text in quotes to keep "
                        <code>"\"3\""</code>" as text."
                    </p>
                </div>
            </header>
            <fieldset disabled={move || context.blocked()}>
                <div class="table-wrap">
                    <table class="table">
                        <thead>
                            <tr>
                                <th>"Name"</th>
                                <th>"Default"</th>
                                {move || {
                                    columns
                                        .get()
                                        .into_iter()
                                        .map(|environment| view! { <th>{environment}</th> })
                                        .collect_view()
                                }}
                                <th>"Previews"</th>
                                <th></th>
                            </tr>
                        </thead>
                        <tbody>
                            <For
                                each={move || rows.indices()}
                                key={|index| *index}
                                children={move |index| variable_row(rows, index, columns)}
                            />
                        </tbody>
                    </table>
                </div>
                {add_button(
                    "Add variable",
                    || false,
                    move || rows.update(|rows| rows.push(VariableDraft::default())),
                )}
                {draft.actions(save, || false)}
            </fieldset>
        </section>
    }
}

/// Inputs of variable `index`: its name, default, value in each of the
/// `columns` environments, and value in previews.
fn variable_row(
    rows: RwSignal<Vec<VariableDraft>>,
    index: usize,
    columns: Memo<Vec<String>>,
) -> AnyView {
    view! {
        <tr>
            {cell(
                rows,
                index,
                "Variable name".into(),
                "domain",
                |row| row.name.clone(),
                |row, value| row.name = value,
            )}
            {cell(
                rows,
                index,
                "Default value".into(),
                "No default",
                |row| row.default.clone(),
                |row, value| row.default = value,
            )}
            {move || {
                columns
                    .get()
                    .into_iter()
                    .map(|environment| {
                        let read = environment.clone();
                        cell(
                            rows,
                            index,
                            format!("Value in {environment}"),
                            "Default",
                            move |row| row.environments.get(&read).cloned().unwrap_or_default(),
                            move |row, value| {
                                row.environments.insert(environment.clone(), value);
                            },
                        )
                    })
                    .collect_view()
            }}
            {cell(
                rows,
                index,
                "Value in previews".into(),
                "Default",
                |row| row.previews.clone(),
                |row, value| row.previews = value,
            )} <td class="actions">{remove_button(move || rows.remove(index))}</td>
        </tr>
    }
    .into_any()
}

/// Text input editing what `read` and `write` select of variable `index`.
fn cell(
    rows: RwSignal<Vec<VariableDraft>>,
    index: usize,
    label: String,
    placeholder: &'static str,
    read: impl Fn(&VariableDraft) -> String + Send + Sync + 'static,
    write: impl Fn(&mut VariableDraft, String) + 'static,
) -> AnyView {
    view! {
        <td>
            <input
                type="text"
                aria-label={label}
                placeholder={placeholder}
                prop:value={move || rows.field(index, &read)}
                on:input={move |event| {
                    let value = event_target_value(&event);
                    rows.edit(index, |row| write(row, value));
                }}
            />
        </td>
    }
    .into_any()
}

/// The value of each variable in the manifest the shown environment deploys:
/// its last fetched one when it follows a branch, else the saved one.
#[component]
pub(super) fn EnvironmentVariables() -> impl IntoView {
    let context = editor();
    let signals = context.signals();
    let values = move || {
        let environment = context.selected_environment()?;
        Some(
            context
                .environment_manifest()?
                .values(&environment.target()),
        )
    };
    view! {
        <section class="card card-flush">
            <header>
                <div>
                    <h3>"Variables"</h3>
                    <p>"Values this environment's next deployment renders its references with."</p>
                </div>
            </header>
            {move || {
                let source = context.selected_environment().map(|environment| environment.source);
                let fetched = source
                    .as_ref()
                    .is_none_or(|source| *source == EnvironmentSource::Saved)
                    || signals
                        .detail
                        .with(|detail| {
                            detail.as_ref().is_some_and(|detail| detail.manifest.is_some())
                        });
                if !fetched {
                    return empty(
                        if source.as_ref().is_some_and(|source| source.promoted_from().is_some()) {
                            "Promote a release into this environment to see its manifest's values."
                        } else {
                            "Deploy this environment to fetch its manifest from its branch."
                        },
                    );
                }
                let values = values().unwrap_or_default();
                if values.is_empty() {
                    return empty("The manifest declares no variables.");
                }
                view! {
                    <table class="table">
                        <tbody>
                            {values
                                .into_iter()
                                .map(|(name, value)| {
                                    view! {
                                        <tr>
                                            <td>
                                                <code>{format!("vars.{name}")}</code>
                                            </td>
                                            <td>
                                                {match value {
                                                    Some(value) => {
                                                        view! { <code>{value.to_string()}</code> }.into_any()
                                                    }
                                                    None => badge(Tone::Warn, "No value").into_any(),
                                                }}
                                            </td>
                                        </tr>
                                    }
                                })
                                .collect_view()}
                        </tbody>
                    </table>
                }
                    .into_any()
            }}
        </section>
    }
}
