//! Previews: disposable deployments of one branch of the application's
//! manifest repository, listed with their status, where their branch is, and
//! whether pushes redeploy them. Each opens on the environment page, with the
//! actions and cards below.
use super::super::ui::{
    Icon, Modal, Tone, badge, empty, health_badge, icon, loaded, notice, text_input,
};
use super::super::{UserError, confirm};
use super::environments::{last_synced, sync_badge};
use super::{EditorContext, editor};
use crate::state::ApplicationHealth;
use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::components::A;
use leptos_router::hooks::use_navigate;
use piqueld_client::sync::short_commit;
use piqueld_client::{
    BranchState, Client, CreatePreviewRequest, DeploymentOrigin, DiagnosticView, EnvironmentView,
    PreviewView, PrunePreviewsRequest, access::AppPermission,
};

impl EditorContext {
    /// Dashboard address of one of the application's previews.
    fn preview_href(self, preview: &str) -> String {
        format!("/dashboard/applications/{}/previews/{preview}", self.id())
    }

    /// Confirms, then deletes `preview` with every volume it created and runs
    /// `deleted` once accepted.
    fn delete_preview(self, preview: &EnvironmentView, deleted: impl FnOnce() + 'static) {
        let branch = preview
            .preview()
            .map(|identity| identity.branch.to_string())
            .unwrap_or_default();
        let message = format!(
            "Delete preview {} of branch {branch}, its services, secrets, and deployment history? Every Docker volume it created is removed with its data.",
            preview.name
        );
        if !confirm(&message) {
            return;
        }
        let id = preview.id.to_string();
        self.mutate_once(
            async move |client| client.delete_preview(&id).await,
            move |_| {
                self.notice.set(
                    "Preview deletion accepted. Its volumes and their data will be removed.".into(),
                );
                self.refresh_dashboard();
                deleted();
            },
        );
    }
}

/// Badge for where a preview's branch is, with the commits or the reason the
/// repository could not be read.
fn branch_state(state: &BranchState) -> (AnyView, Option<String>) {
    let (tone, detail) = match state {
        BranchState::Exists { head } => (Tone::Ok, Some(format!("at {}", short_commit(head)))),
        BranchState::Moved { head, deployed } => (
            Tone::Warn,
            Some(format!(
                "to {}, deployed {}",
                short_commit(head),
                short_commit(deployed)
            )),
        ),
        BranchState::Gone => (Tone::Bad, None),
        // The badge says it runs a local snapshot; Deploys details it.
        BranchState::Local { head } => (
            Tone::Neutral,
            Some(head.as_ref().map_or_else(
                || "never pushed".into(),
                |head| format!("branch at {}", short_commit(head)),
            )),
        ),
        BranchState::Unknown { message } => (Tone::Neutral, Some(message.clone())),
    };
    (badge(tone, state.to_string()), detail)
}

/// What a preview's latest deployment deploys: its branch head, or a local
/// snapshot with its commit and base, and what Redeploy does then.
fn deploys(view: &PreviewView) -> AnyView {
    let DeploymentOrigin::Local(snapshot) = &view.origin else {
        return view! { "The head of its branch" }.into_any();
    };
    let base = snapshot.base.as_ref().map_or_else(
        || view! { ", uploaded with its whole history" }.into_any(),
        |base| {
            view! {
                " on "
                <code title={base.clone()}>{short_commit(base).to_owned()}</code>
            }
            .into_any()
        },
    );
    let redeploy = match &view.branch {
        BranchState::Local { head: None } => format!(
            "Its branch was never pushed, so Redeploy branch head fails until it is. Deploy a new snapshot from its worktree with `{}`.",
            local_command(&view.preview)
        ),
        _ => "Redeploy branch head replaces it with the head of its branch.".into(),
    };
    view! {
        "Local snapshot "
        <code title={snapshot.commit.clone()}>{short_commit(&snapshot.commit).to_owned()}</code>
        {base}
        <div class="muted">{redeploy}</div>
    }
    .into_any()
}

/// The command that deploys a snapshot of a worktree to `preview`, run
/// from that worktree on its branch.
///
/// ```text
/// piquelctl preview deploy app-0123 --local --slot agent-2
/// ```
fn local_command(preview: &EnvironmentView) -> String {
    let slot = preview
        .preview()
        .and_then(|identity| identity.slot.as_ref())
        .map_or_else(String::new, |slot| format!(" --slot {slot}"));
    format!(
        "piquelctl preview deploy {} --local{slot}",
        preview.application_id
    )
}

/// Badge for a preview whose deployed target `[previews]` bounded, with how
/// in its title: services given default limits or fewer replicas.
fn bounds_badge(bounds: &[DiagnosticView]) -> Option<AnyView> {
    (!bounds.is_empty()).then(|| {
        let title = bounds
            .iter()
            .map(|bound| bound.message.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        view! { <span title={title}>{badge(Tone::Neutral, "Bounded")}</span> }.into_any()
    })
}

/// Lists how `[previews]` bounded a preview's deployed target.
fn bounds(bounds: Vec<DiagnosticView>) -> AnyView {
    if bounds.is_empty() {
        return view! { <span class="muted">"None"</span> }.into_any();
    }
    bounds
        .into_iter()
        .map(|bound| view! { <div>{bound.message}</div> })
        .collect_view()
        .into_any()
}

/// The application's previews with their status and branch state, read when
/// the tab opens and on refresh. "Prune gone" deletes those whose branch is
/// gone, with `previews:delete`.
#[component]
#[expect(clippy::too_many_lines, reason = "flat view! markup")]
pub(super) fn PreviewList() -> impl IntoView {
    let context = editor();
    // Listing runs a `git ls-remote` of the manifest repository, so it is
    // fetched on demand rather than polled.
    let data = LocalResource::new(move || {
        let id = context.id();
        async move { Client::browser().previews(&id).await.user_error() }
    });
    // Previews whose branch was gone when the list loaded.
    let gone = move || {
        data.get()
            .and_then(Result::ok)
            .unwrap_or_default()
            .into_iter()
            .filter(|view| view.branch == BranchState::Gone)
            .map(|view| view.preview)
            .collect::<Vec<_>>()
    };
    // The daemon checks each branch again and keeps any no longer gone.
    let prune = move |_| {
        let previews = gone();
        let names = previews
            .iter()
            .map(|preview| preview.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let message = format!(
            "Delete the previews whose branch is gone ({names}), their services, secrets, and deployment history? Every Docker volume they created is removed with its data."
        );
        if !confirm(&message) {
            return;
        }
        let request = PrunePreviewsRequest {
            previews: previews.into_iter().map(|preview| preview.id).collect(),
        };
        let application = context.id();
        context.mutate_once(
            async move |client| client.prune_previews(&application, &request).await,
            move |deleted| {
                context.notice.set(format!(
                    "Deletion of {} previews accepted. Their volumes and their data will be removed.",
                    deleted.len()
                ));
                context.refresh_dashboard();
                data.refetch();
            },
        );
    };
    let none = move || {
        if context.repository_backed() {
            "No previews yet. Create one to deploy a branch."
        } else {
            "No previews. Previews deploy branches of the application's manifest repository: connect one in Source first."
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
                <div class="btn-group">
                    <button type="button" class="btn" on:click={move |_| data.refetch()}>
                        {icon(Icon::Refresh)}
                        "Refresh"
                    </button>
                    <button
                        type="button"
                        class="btn btn-danger"
                        title="Delete every preview whose branch is gone"
                        hidden={move || !context.can(AppPermission::PreviewsDelete)}
                        disabled={move || context.blocked() || gone().is_empty()}
                        on:click={prune}
                    >
                        "Prune gone"
                    </button>
                    <NewPreview />
                </div>
            </div>
            {loaded(
                data,
                "Loading previews…",
                move |previews| {
                    if previews.is_empty() {
                        return empty(none());
                    }
                    view! {
                        <ul class="list" aria-label="Previews">
                            {previews
                                .into_iter()
                                .map(|view| {
                                    view! {
                                        <PreviewRow
                                            view={view}
                                            list={data}
                                        />
                                    }
                                })
                                .collect_view()}
                        </ul>
                    }
                        .into_any()
                },
            )}
        </div>
    }
}

/// "New preview" button and dialog. Creates and deploys a preview of a branch,
/// or finds the existing one of that branch and slot, then opens its
/// deployments. The button is shown with `previews:deploy`.
#[component]
fn NewPreview() -> impl IntoView {
    let context = editor();
    let opened = RwSignal::new(false);
    let form = RwSignal::new((String::new(), String::new()));
    let navigate = use_navigate();
    let submit = move |event: leptos::ev::SubmitEvent| {
        event.prevent_default();
        let (branch, slot) = form.get_untracked();
        let request = CreatePreviewRequest {
            branch: branch.trim().to_owned(),
            slot: Some(slot.trim().to_owned()).filter(|slot| !slot.is_empty()),
        };
        let application = context.id();
        let navigate = navigate.clone();
        context.mutate(
            move |client| {
                let (application, request) = (application.clone(), request.clone());
                async move { client.create_preview(&application, &request).await }
            },
            move |created| {
                opened.set(false);
                form.set(Default::default());
                let href = context.preview_href(created.preview.id.as_str());
                navigate(
                    &format!("{href}?tab=deployments"),
                    NavigateOptions::default(),
                );
            },
        );
    };
    view! {
        <button
            type="button"
            class="btn btn-primary"
            hidden={move || !context.can(AppPermission::PreviewsDeploy)}
            disabled={move || context.blocked() || !context.repository_backed()}
            on:click={move |_| opened.set(true)}
        >
            {icon(Icon::Plus)}
            "New preview"
        </button>
        <Modal
            title="Create preview"
            opened={opened}
            busy={context.busy}
            on_close={Callback::new(move |()| form.set(Default::default()))}
        >
            <form class="stack-sm" on:submit={submit}>
                <fieldset class="stack-sm" disabled={move || context.blocked()}>
                    <p class="hint">
                        "Deploys the manifest on a branch of the application's repository, with the preview variables and visibility. A slot tells apart several previews of one branch, such as one per agent working on it. A preview of that branch and slot that already exists is opened without redeploying."
                    </p>
                    {text_input("Branch", form, |v| v.0.clone(), |v, input| v.0 = input)}
                    {text_input("Slot (optional)", form, |v| v.1.clone(), |v, input| v.1 = input)}
                </fieldset>
                {move || context.error.get().map(|error| notice(Tone::Bad, error))}
                <div class="form-actions">
                    <button
                        type="submit"
                        class="btn btn-primary"
                        disabled={move || context.blocked()}
                    >
                        "Create preview"
                    </button>
                </div>
            </form>
        </Modal>
    }
}

/// One preview's row, opening its page, with its slug, slot, branch state,
/// sync state with the head it last synced, and status. Delete, shown with
/// `previews:delete`, reloads `list` once accepted.
#[component]
fn PreviewRow(
    view: PreviewView,
    list: LocalResource<Result<Vec<PreviewView>, String>>,
) -> impl IntoView {
    let context = editor();
    let preview = StoredValue::new(view.preview.clone());
    let name = view.preview.name.to_string();
    let identity = view.preview.preview();
    let branch = identity
        .map(|identity| identity.branch.to_string())
        .unwrap_or_default();
    let subtitle = match identity.and_then(|identity| identity.slot.as_ref()) {
        Some(slot) => format!("{name} · slot {slot}"),
        None => name.clone(),
    };
    let (branch_badge, branch_detail) = branch_state(&view.branch);
    let bounded = bounds_badge(&view.bounds);
    let (sync, sync_reason) = sync_badge(view.sync);
    let synced = view.preview.synced.clone().map(|head| {
        view! { <span class="meta">"synced " {last_synced(Some(head))}</span> }
    });
    let deleting = view.preview.delete_intent;
    let status = if deleting {
        badge(Tone::Warn, "Deleting")
    } else {
        health_badge(ApplicationHealth::from_server_state(view.status.state))
    };
    view! {
        <li class="list-row list-row-link">
            <span class="title">
                <A href={context.preview_href(view.preview.id.as_str())}>{branch}</A>
                <small>{subtitle}</small>
            </span>
            <span class="meta">{branch_detail}</span>
            {bounded}
            {synced}
            {branch_badge}
            <span title={sync_reason}>{sync}</span>
            <span title={view.status.message}>{status}</span>
            <button
                type="button"
                class="btn btn-danger btn-sm"
                aria-label={format!("Delete preview {name}")}
                hidden={move || !context.can(AppPermission::PreviewsDelete)}
                disabled={move || context.blocked() || deleting}
                on:click={move |_| {
                    preview
                        .with_value(|preview| {
                            context.delete_preview(preview, move || list.refetch());
                        });
                }}
            >
                "Delete"
            </button>
            <span class="chevron" aria-hidden="true">
                {icon(Icon::ChevronRight)}
            </span>
        </li>
    }
}

/// "Redeploy branch head" button of a preview's page: deploys the head of
/// its branch again, replacing any local snapshot it runs, then shows its
/// deployments. Snapshots are only deployed from a worktree, by
/// `piquelctl preview deploy <app> --local`. Hidden without `previews:deploy`.
#[component]
pub(super) fn PreviewActions() -> impl IntoView {
    let context = editor();
    let deploy = move |_| {
        let id = context.environment_id();
        context.mutate(
            move |client| {
                let id = id.clone();
                async move { client.deploy_preview(&id).await }
            },
            move |_| {
                context
                    .notice
                    .set("Deployment accepted. Follow its progress below.".into());
                context.tab.set("Deployments");
                context.refresh_dashboard();
            },
        );
    };
    view! {
        <button
            type="button"
            class="btn btn-primary"
            title={move || {
                let command = context
                    .selected_environment()
                    .map_or_else(String::new, |preview| local_command(&preview));
                format!(
                    "Deploy the head of this preview's branch from the repository, replacing a local snapshot it runs. Deploy a new snapshot from its worktree with `{command}`.",
                )
            }}
            hidden={move || !context.can(AppPermission::PreviewsDeploy)}
            disabled={move || context.environment_action_blocked()}
            on:click={deploy}
        >
            {icon(Icon::Rocket)}
            "Redeploy branch head"
        </button>
    }
}

/// A preview's branch, slot, slug, and ID, with its branch state,
/// `[previews]` bounds and sync state read when the page opens and on
/// refresh. Its URLs are in the runtime overview, as for environments.
#[component]
pub(super) fn PreviewSettings() -> impl IntoView {
    let context = editor();
    let id = context.environment_id();
    let identity = context
        .selected_environment()
        .and_then(|preview| preview.preview().cloned());
    // Reading the branch state runs a `git ls-remote`, so it is not polled.
    let data = LocalResource::new({
        let id = id.clone();
        move || {
            let id = id.clone();
            async move { Client::browser().preview(&id).await.user_error() }
        }
    });
    // `show` of the loaded preview; muted text while it loads or if loading failed.
    let detail = move |show: fn(PreviewView) -> AnyView| {
        move || match data.get() {
            None => view! { <span class="muted">"Loading…"</span> }.into_any(),
            Some(Err(error)) => view! { <span class="muted">{error}</span> }.into_any(),
            Some(Ok(view)) => show(view),
        }
    };
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>"Preview"</h3>
                    <p>
                        "Deploys its branch with the preview variables and visibility. Its slug comes from its branch and slot, and never changes."
                    </p>
                </div>
                <button type="button" class="btn btn-sm" on:click={move |_| data.refetch()}>
                    {icon(Icon::Refresh)}
                    "Refresh"
                </button>
            </header>
            <dl class="kv">
                <dt>"Branch"</dt>
                <dd>
                    <code>
                        {identity
                            .as_ref()
                            .map(|identity| identity.branch.to_string())
                            .unwrap_or_default()}
                    </code>
                </dd>
                <dt>"Slot"</dt>
                <dd>
                    {identity
                        .as_ref()
                        .and_then(|identity| identity.slot.as_ref())
                        .map_or_else(|| "—".to_owned(), ToString::to_string)}
                </dd>
                <dt>"Slug"</dt>
                <dd>
                    <code>
                        {identity
                            .as_ref()
                            .map(|identity| identity.slug.to_string())
                            .unwrap_or_default()}
                    </code>
                </dd>
                <dt>"Preview ID"</dt>
                <dd>
                    <code>{id}</code>
                </dd>
                <dt>"Branch state"</dt>
                <dd>
                    {detail(|view| {
                        let (badge, detail) = branch_state(&view.branch);
                        view! {
                            {badge}
                            {detail.map(|detail| view! { <div class="muted">{detail}</div> })}
                        }
                            .into_any()
                    })}
                </dd>
                <dt>"Deploys"</dt>
                <dd>{detail(|view| deploys(&view))}</dd>
                <dt>"Deploy on push"</dt>
                <dd>
                    {detail(|view| {
                        let (badge, reason) = sync_badge(view.sync);
                        view! {
                            {badge}
                            <div class="muted">{reason}</div>
                        }
                            .into_any()
                    })}
                </dd>
                <dt>"Last synced"</dt>
                <dd>{detail(|view| last_synced(view.preview.synced))}</dd>
                <dt>"Bounds"</dt>
                <dd>{detail(|view| bounds(view.bounds))}</dd>
            </dl>
        </section>
    }
}

/// Danger-zone card of a preview's page: deletes it with every volume it
/// created, then returns to the previews. Hidden without `previews:delete`.
#[component]
pub(super) fn DeletePreview() -> impl IntoView {
    let context = editor();
    let navigate = use_navigate();
    let deleting = move || {
        context
            .selected_environment()
            .is_none_or(|preview| preview.delete_intent)
    };
    let delete = move |_| {
        let Some(preview) = context.selected_environment() else {
            return;
        };
        let navigate = navigate.clone();
        let href = format!("/dashboard/applications/{}?tab=previews", context.id());
        context.delete_preview(&preview, move || {
            navigate(&href, NavigateOptions::default());
        });
    };
    view! {
        <section
            class="card card-danger"
            hidden={move || !context.can(AppPermission::PreviewsDelete)}
        >
            <header>
                <div>
                    <h3>"Delete preview"</h3>
                    <p>
                        "Removes this preview's running services, secrets, and deployment history, and every Docker volume it created with its data."
                    </p>
                </div>
                <button
                    type="button"
                    class="btn btn-danger"
                    disabled={move || context.blocked() || deleting()}
                    on:click={delete}
                >
                    "Delete preview"
                </button>
            </header>
        </section>
    }
}
