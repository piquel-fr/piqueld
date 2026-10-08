//! Previews: disposable deployments of one branch of the application's
//! manifest repository, listed with their status and where their branch is.
use super::super::client_error_message;
use super::super::ui::{Icon, Tone, badge, empty, health_badge, icon, notice};
use super::editor;
use crate::state::ApplicationHealth;
use leptos::prelude::*;
use piqueld_client::{BranchState, Client, PreviewView};

/// Badge for where a preview's branch is, with the commits or the reason the
/// repository could not be read below it.
fn branch_state(state: &BranchState) -> AnyView {
    // Full commit IDs are too wide for a table cell.
    let short = |commit: &str| commit.get(..12).unwrap_or(commit).to_owned();
    let (tone, detail) = match state {
        BranchState::Exists { head } => (Tone::Ok, Some(format!("at {}", short(head)))),
        BranchState::Moved { head, deployed } => (
            Tone::Warn,
            Some(format!("to {}, deployed {}", short(head), short(deployed))),
        ),
        BranchState::Gone => (Tone::Bad, None),
        BranchState::Unknown { message } => (Tone::Neutral, Some(message.clone())),
    };
    view! {
        {badge(tone, state.to_string())}
        {detail.map(|detail| view! { <div class="muted">{detail}</div> })}
    }
    .into_any()
}

/// The application's previews with their status, URLs, and branch state, read
/// when the tab opens and on refresh.
#[component]
pub(super) fn PreviewList() -> impl IntoView {
    let context = editor();
    // Listing runs a `git ls-remote` of the manifest repository, so it is
    // fetched on demand rather than polled.
    let data = LocalResource::new(move || {
        let id = context.id();
        async move {
            Client::browser()
                .previews(&id)
                .await
                .map_err(|error| client_error_message(&error))
        }
    });
    let none = move || {
        let command = format!(
            "piquelctl preview create {} --branch <branch>",
            context.name()
        );
        if context.managed() {
            format!("No previews. Create one with `{command}`.")
        } else {
            format!(
                "No previews. Previews deploy branches of the application's manifest repository: connect one in Source, then run `{command}`."
            )
        }
    };
    view! {
        <div class="stack">
            <div class="section-header">
                <div>
                    <h2>"Previews"</h2>
                    <p>
                        "Disposable deployments of one branch of the manifest repository, each with its own secrets, volumes, and history."
                    </p>
                </div>
                <button type="button" class="btn btn-sm" on:click={move |_| data.refetch()}>
                    {icon(Icon::Refresh)}
                    "Refresh"
                </button>
            </div>
            {move || match data.get() {
                None => empty("Loading previews…"),
                Some(Err(error)) => notice(Tone::Bad, error),
                Some(Ok(previews)) if previews.is_empty() => empty(none()),
                Some(Ok(previews)) => {
                    view! {
                        <div class="table-wrap">
                            <table class="table">
                                <thead>
                                    <tr>
                                        <th>"Branch"</th>
                                        <th>"Slot"</th>
                                        <th>"Slug"</th>
                                        <th>"Status"</th>
                                        <th>"URLs"</th>
                                        <th>"Branch state"</th>
                                        <th></th>
                                    </tr>
                                </thead>
                                <tbody>
                                    {previews
                                        .into_iter()
                                        .map(|view| {
                                            view! {
                                                <PreviewRow
                                                    view={view}
                                                    deleted={Callback::new(move |()| data.refetch())}
                                                />
                                            }
                                        })
                                        .collect_view()}
                                </tbody>
                            </table>
                        </div>
                    }
                        .into_any()
                }
            }}
        </div>
    }
}

/// One preview's row. Delete confirms, then deletes the preview with every
/// volume it created and runs `deleted` once accepted.
#[component]
fn PreviewRow(view: PreviewView, deleted: Callback<()>) -> impl IntoView {
    let context = editor();
    let id = StoredValue::new(view.preview.id.to_string());
    let name = view.preview.name.to_string();
    let preview = view.preview.preview().cloned();
    let branch = preview
        .as_ref()
        .map(|preview| preview.branch.to_string())
        .unwrap_or_default();
    let deleting = view.preview.delete_intent;
    let message = format!(
        "Delete preview {name} of branch {branch}, its services, secrets, and deployment history? Every Docker volume it created is removed with its data."
    );
    let delete = move |_| {
        if !window().confirm_with_message(&message).unwrap_or(false) {
            return;
        }
        context.mutate(
            move |client| {
                let id = id.get_value();
                async move { client.delete_preview(&id).await }
            },
            move |_| {
                context.notice.set(
                    "Preview deletion accepted. Its volumes and their data will be removed.".into(),
                );
                deleted.run(());
            },
        );
    };
    let status = if deleting {
        badge(Tone::Warn, "Deleting")
    } else {
        health_badge(ApplicationHealth::from_server_state(view.status.state))
    };
    let urls = if view.hostnames.is_empty() {
        view! { <span class="muted">"None"</span> }.into_any()
    } else {
        view.hostnames
            .into_iter()
            .map(|hostname| {
                let href = format!("https://{hostname}");
                view! {
                    <div>
                        <a href={href} target="_blank" rel="noopener noreferrer">
                            {hostname}
                        </a>
                    </div>
                }
            })
            .collect_view()
            .into_any()
    };
    view! {
        <tr>
            <td>
                <strong>{branch}</strong>
            </td>
            <td class="muted">
                {preview
                    .and_then(|preview| preview.slot)
                    .map_or_else(|| "—".to_owned(), |slot| slot.to_string())}
            </td>
            <td>
                <code>{name.clone()}</code>
            </td>
            <td>
                {status}
                {view.status.message.map(|message| view! { <div class="muted">{message}</div> })}
            </td>
            <td>{urls}</td>
            <td>{branch_state(&view.branch)}</td>
            <td class="actions">
                <button
                    type="button"
                    class="btn btn-danger btn-sm"
                    aria-label={format!("Delete preview {name}")}
                    disabled={move || context.blocked() || deleting}
                    on:click={delete}
                >
                    "Delete"
                </button>
            </td>
        </tr>
    }
}
