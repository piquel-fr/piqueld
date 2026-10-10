//! Manifest variables: defaults and per-environment values on the application
//! page, and the values one environment renders with on its page.
use super::super::ui::{Icon, Tone, badge, empty, icon, remove_button};
use super::{dirty_group, editor, save_actions};
use leptos::prelude::*;
use piqueld_client::{
    EnvironmentSource, Variable,
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
    let saved = move || Variables::of(&context.manifest());
    let draft = RwSignal::new(VariableDraft::rows(&saved()));
    let baseline = RwSignal::new(draft.get_untracked());
    dirty_group("variables".into(), draft, baseline);
    Effect::new(move |_| {
        let rows = VariableDraft::rows(&saved());
        if draft.get_untracked() == baseline.get_untracked() {
            draft.set(rows.clone());
            baseline.set(rows);
        }
    });
    let columns = Memo::new(move |_| {
        let mut names = context.saved.with(|saved| {
            saved
                .environments
                .iter()
                .map(|environment| environment.name.to_string())
                .collect::<BTreeSet<_>>()
        });
        names.extend(saved().environments.into_keys());
        names.into_iter().collect::<Vec<_>>()
    });
    let save = move || {
        let variables = match VariableDraft::variables(&draft.get_untracked()) {
            Ok(variables) => variables,
            Err(message) => {
                context.error.set(Some(message));
                return;
            }
        };
        context.save(
            ApplicationEdit::Variables(variables),
            Callback::new(move |saved: piqueld_client::ApplicationView| {
                draft.set(VariableDraft::rows(&Variables::of(
                    &saved.application.to_manifest(),
                )));
                baseline.set(draft.get_untracked());
            }),
        );
    };
    let cell = move |index: usize, read: fn(&VariableDraft) -> String| {
        draft.with(|rows| rows.get(index).map(read).unwrap_or_default())
    };
    let update = move |index: usize, write: Box<dyn FnOnce(&mut VariableDraft)>| {
        draft.update(|rows| {
            if let Some(row) = rows.get_mut(index) {
                write(row);
            }
        });
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
                                each={move || (0..draft.with(Vec::len)).collect::<Vec<_>>()}
                                key={|index| *index}
                                children={move |index| {
                                    view! {
                                        <tr>
                                            <td>
                                                <input
                                                    type="text"
                                                    aria-label="Variable name"
                                                    placeholder="domain"
                                                    prop:value={move || cell(index, |row| row.name.clone())}
                                                    on:input={move |event| {
                                                        let value = event_target_value(&event);
                                                        update(index, Box::new(move |row| row.name = value));
                                                    }}
                                                />
                                            </td>
                                            <td>
                                                <input
                                                    type="text"
                                                    aria-label="Default value"
                                                    placeholder="No default"
                                                    prop:value={move || cell(index, |row| row.default.clone())}
                                                    on:input={move |event| {
                                                        let value = event_target_value(&event);
                                                        update(index, Box::new(move |row| row.default = value));
                                                    }}
                                                />
                                            </td>
                                            {move || {
                                                columns
                                                    .get()
                                                    .into_iter()
                                                    .map(|environment| {
                                                        let read = environment.clone();
                                                        let label = format!("Value in {environment}");
                                                        view! {
                                                            <td>
                                                                <input
                                                                    type="text"
                                                                    aria-label={label}
                                                                    placeholder="Default"
                                                                    prop:value={move || {
                                                                        draft
                                                                            .with(|rows| {
                                                                                rows.get(index)
                                                                                    .and_then(|row| row.environments.get(&read).cloned())
                                                                                    .unwrap_or_default()
                                                                            })
                                                                    }}
                                                                    on:input={move |event| {
                                                                        let value = event_target_value(&event);
                                                                        let environment = environment.clone();
                                                                        update(
                                                                            index,
                                                                            Box::new(move |row| {
                                                                                row.environments.insert(environment, value);
                                                                            }),
                                                                        );
                                                                    }}
                                                                />
                                                            </td>
                                                        }
                                                    })
                                                    .collect_view()
                                            }}
                                            <td>
                                                <input
                                                    type="text"
                                                    aria-label="Value in previews"
                                                    placeholder="Default"
                                                    prop:value={move || cell(index, |row| row.previews.clone())}
                                                    on:input={move |event| {
                                                        let value = event_target_value(&event);
                                                        update(index, Box::new(move |row| row.previews = value));
                                                    }}
                                                />
                                            </td>
                                            <td class="actions">
                                                {remove_button(move || {
                                                    draft
                                                        .update(|rows| {
                                                            rows.remove(index);
                                                        });
                                                })}
                                            </td>
                                        </tr>
                                    }
                                }}
                            />
                        </tbody>
                    </table>
                </div>
                <button
                    type="button"
                    class="btn btn-sm"
                    on:click={move |_| draft.update(|rows| rows.push(VariableDraft::default()))}
                >
                    {icon(Icon::Plus)}
                    "Add variable"
                </button>
                {save_actions(draft, baseline, save, || false)}
            </fieldset>
        </section>
    }
}

/// The value of each variable in the manifest the shown environment deploys:
/// its last fetched one when it follows a branch, else the saved one.
#[component]
pub(super) fn EnvironmentVariables() -> impl IntoView {
    let context = editor();
    let signals = context.dashboard.with_value(|d| d.signals);
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
                let fetched = source.as_ref().is_none_or(|source| *source == EnvironmentSource::Saved)
                    || signals.detail.with(|detail| {
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
                                                    Some(value) => view! { <code>{value.to_string()}</code> }.into_any(),
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
