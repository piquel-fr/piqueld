//! Grant display and editing for accounts and invitations.
//!
//! The editor selects permissions and one scope (every application, or chosen
//! ones) shared by all application permissions; installation-wide permissions
//! always apply everywhere. Presets only fill in the selection.
use super::ui::{Tone, notice};
use leptos::prelude::*;
use piqueld_client::{
    ApplicationId,
    access::{Grant, Grants, Permission, Preset, Scope},
};
use std::collections::BTreeSet;

/// The signed-in credential's grants.
pub(super) fn grants() -> Signal<Grants> {
    let session = super::auth::session();
    Signal::derive(move || {
        session
            .get()
            .map(|session| session.grants)
            .unwrap_or_default()
    })
}

/// Whether the signed-in credential holds `permission` on some application.
pub(super) fn can(permission: Permission) -> Signal<bool> {
    let grants = grants();
    Signal::derive(move || grants.with(|grants| grants.require(permission).is_ok()))
}

/// Application name for an ID from the dashboard's application list, or the ID.
fn application_name(id: &ApplicationId) -> String {
    super::dashboard_context()
        .signals
        .applications
        .with_untracked(|rows| {
            rows.iter()
                .find(|row| row.application.id == *id)
                .map(|row| row.application.name.to_string())
        })
        .unwrap_or_else(|| id.to_string())
}

/// One line per grant, e.g. `apps:deploy on blog, shop`.
pub(super) fn describe(grants: &Grants) -> Vec<String> {
    grants
        .to_list()
        .iter()
        .map(|grant: &Grant| match &grant.applications {
            None if grant.permission.scopable() => {
                format!("{} on every application", grant.permission)
            }
            None => grant.permission.to_string(),
            Some(ids) => format!(
                "{} on {}",
                grant.permission,
                ids.iter()
                    .map(application_name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })
        .collect()
}

/// A compact list of grants, or "No access".
pub(super) fn summary(grants: &Grants) -> AnyView {
    let lines = describe(grants);
    if lines.is_empty() {
        return view! { <span class="muted">"No access"</span> }.into_any();
    }
    view! {
        <ul class="grant-list">
            {lines.into_iter().map(|line| view! { <li>{line}</li> }).collect_view()}
        </ul>
    }
    .into_any()
}

/// Builds grants from a selection: application permissions on `scope`,
/// installation-wide ones everywhere.
fn build(permissions: &BTreeSet<Permission>, scope: &Scope) -> Grants {
    let mut grants = Grants::default();
    for permission in permissions {
        grants.grant_within(*permission, scope);
    }
    grants
}

/// Edits grants into `value`, starting from `initial`. `value` keeps
/// `initial` until the selection changes.
#[component]
pub(super) fn GrantEditor(initial: Grants, value: RwSignal<Grants>) -> impl IntoView {
    let list = initial.to_list();
    let scopes: BTreeSet<_> = list
        .iter()
        .filter(|grant| grant.permission.scopable())
        .map(|grant| grant.applications.clone())
        .collect();
    // Grants limited to different applications per permission cannot be shown
    // with one scope; changing the selection replaces them with it.
    let mixed = scopes.len() > 1;
    let permissions = RwSignal::new(
        list.iter()
            .map(|grant| grant.permission)
            .collect::<BTreeSet<_>>(),
    );
    let chosen: BTreeSet<ApplicationId> = scopes.iter().flatten().flatten().cloned().collect();
    let everywhere = RwSignal::new(scopes.iter().all(Option::is_none));
    let applications = RwSignal::new(chosen);
    let scope = move || {
        if everywhere.get() {
            Scope::All
        } else {
            Scope::Only(applications.get())
        }
    };
    value.set(initial);
    Effect::new(move |previous: Option<()>| {
        let grants = build(&permissions.get(), &scope());
        if previous.is_some() {
            value.set(grants);
        }
    });
    let rows = super::dashboard_context().signals.applications;
    let apply_preset = move |name: String| {
        if let Some(preset) = Preset::parse(&name) {
            permissions.set(preset.permissions(everywhere.get_untracked()).collect());
        }
    };
    view! {
        <div class="stack-sm">
            {mixed
                .then(|| {
                    notice(
                        Tone::Warn,
                        "These grants cover different applications per permission. Changing the selection below applies it to every application permission.",
                    )
                })}
            <label class="field">
                <span>"Preset"</span>
                <select on:change={move |event| apply_preset(event_target_value(&event))}>
                    <option value="">"Choose a preset…"</option>
                    {Preset::ALL
                        .iter()
                        .map(|preset| {
                            view! { <option value={preset.as_str()}>{preset.as_str()}</option> }
                        })
                        .collect_view()}
                </select>
            </label>
            <div class="grant-permissions">
                {Permission::all()
                    .map(|permission| {
                        view! {
                            <label class="checkbox" title={permission.description()}>
                                <input
                                    type="checkbox"
                                    prop:checked={move || permissions.with(|set| set.contains(&permission))}
                                    on:change={move |event| {
                                        let checked = event_target_checked(&event);
                                        permissions
                                            .update(|set| {
                                                if checked {
                                                    set.insert(permission);
                                                } else {
                                                    set.remove(&permission);
                                                }
                                            });
                                    }}
                                />
                                <code>{permission.as_str()}</code>
                                <span class="muted">{permission.description()}</span>
                            </label>
                        }
                    })
                    .collect_view()}
            </div>
            <label class="checkbox">
                <input
                    type="checkbox"
                    prop:checked={move || everywhere.get()}
                    on:change={move |event| everywhere.set(event_target_checked(&event))}
                />
                "Application permissions apply to every application, including future ones"
            </label>
            <Show when={move || !everywhere.get()}>
                <div class="grant-applications">
                    {move || {
                        rows.get()
                            .into_iter()
                            .map(|row| {
                                let id = row.application.id.clone();
                                let checked_id = id.clone();
                                view! {
                                    <label class="checkbox">
                                        <input
                                            type="checkbox"
                                            prop:checked={move || {
                                                applications.with(|set| set.contains(&checked_id))
                                            }}
                                            on:change={move |event| {
                                                let checked = event_target_checked(&event);
                                                let id = id.clone();
                                                applications
                                                    .update(|set| {
                                                        if checked {
                                                            set.insert(id);
                                                        } else {
                                                            set.remove(&id);
                                                        }
                                                    });
                                            }}
                                        />
                                        {row.application.name.to_string()}
                                    </label>
                                }
                            })
                            .collect_view()
                    }}
                    {move || {
                        applications
                            .with(BTreeSet::is_empty)
                            .then(|| {
                                notice(
                                    Tone::Warn,
                                    "Select at least one application, or application permissions grant nothing.",
                                )
                            })
                    }}
                </div>
            </Show>
        </div>
    }
}
