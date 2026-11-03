//! Grant display and editing for accounts and invitations.
//!
//! The editor selects permissions and one scope (every application, or chosen
//! ones) shared by all application permissions; installation-wide permissions
//! always apply everywhere. Presets only fill in the selection.
use super::ui::{Tone, checkbox, member_checkbox, notice};
use crate::auth::{DescribeGrants, GrantSelection};
use leptos::prelude::*;
use piqueld_client::{
    ApplicationId,
    access::{Grants, Permission, Preset},
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

/// Application name for an ID from the dashboard's application list, or the
/// ID outside the dashboard (e.g. on the CLI approval page) and until the
/// list loads. Tracks the list, so views calling it fill in the names later.
fn application_name(id: &ApplicationId) -> String {
    use_context::<super::DashboardContext>()
        .and_then(|context| {
            context.signals.applications.with(|rows| {
                rows.iter()
                    .find(|row| row.application.id == *id)
                    .map(|row| row.application.name.clone())
            })
        })
        .unwrap_or_else(|| id.to_string())
}

/// A compact list of grants, or "No access", naming applications once the
/// dashboard's application list loads.
pub(super) fn summary(grants: &Grants) -> AnyView {
    let grants = grants.clone();
    (move || {
        let lines = grants.describe(application_name);
        if lines.is_empty() {
            return view! { <span class="muted">"No access"</span> }.into_any();
        }
        view! {
            <ul class="grant-list">
                {lines.into_iter().map(|line| view! { <li>{line}</li> }).collect_view()}
            </ul>
        }
        .into_any()
    })
    .into_any()
}

/// `grants` in a few words: the one grant, or how many there are.
pub(super) fn short_summary(grants: &Grants) -> String {
    let lines = grants.describe(application_name);
    match lines.len() {
        0 => "no access".to_owned(),
        1 => lines.into_iter().next().unwrap_or_default(),
        count => format!("{count} grants"),
    }
}

/// Edits grants into `value`, starting from `initial`. `value` keeps
/// `initial` until the selection changes. With `limit`, the result is
/// clamped to it, e.g. a token to its owner's access, and the effective
/// selection is shown.
#[component]
#[expect(clippy::too_many_lines, reason = "flat view! markup")]
pub(super) fn GrantEditor(
    initial: Grants,
    value: RwSignal<Grants>,
    #[prop(optional)] limit: Option<Grants>,
) -> impl IntoView {
    let GrantSelection {
        permissions,
        everywhere,
        applications,
        mixed,
    } = GrantSelection::from(&initial);
    let permissions = RwSignal::new(permissions);
    let everywhere = RwSignal::new(everywhere);
    let applications = RwSignal::new(applications);
    let clamped = limit.is_some();
    let clamp = move |grants: Grants| match &limit {
        Some(limit) => grants.intersection(limit),
        None => grants,
    };
    value.set(clamp(initial));
    Effect::new(move |previous: Option<()>| {
        let selected = GrantSelection {
            permissions: permissions.get(),
            everywhere: everywhere.get(),
            applications: applications.get(),
            mixed,
        }
        .grants();
        if previous.is_some() {
            value.set(clamp(selected));
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
            {clamped
                .then(|| {
                    view! {
                        <div class="field">
                            <span>"Effective access"</span>
                            {move || summary(&value.get())}
                        </div>
                    }
                })}
            {mixed
                .then(|| {
                    notice(
                        Tone::Warn,
                        "These grants cover different applications per permission. Changing the selection below applies it to every application permission.",
                    )
                })} <label class="field">
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
                                {member_checkbox(permissions, permission)}
                                <code>{permission.as_str()}</code>
                                <span class="muted">{permission.description()}</span>
                            </label>
                        }
                    })
                    .collect_view()}
            </div>
            {checkbox(
                "Application permissions apply to every application, including future ones",
                everywhere,
            )} <Show when={move || !everywhere.get()}>
                <div class="grant-applications">
                    {move || {
                        rows.get()
                            .into_iter()
                            .map(|row| {
                                view! {
                                    <label class="checkbox">
                                        {member_checkbox(applications, row.application.id)}
                                        {row.application.name}
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
