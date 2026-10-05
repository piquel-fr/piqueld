//! Deployment actions, preview dialog, history, and snapshot inspection.
use super::super::format::timestamp;
use super::super::ui::{
    Icon, Modal, Tabs, Tone, badge, empty, icon, notice, operation_badge, when,
};
use super::{client_error_message, editor, mutation_client, transport_failure};
use crate::browser::Alive;
use leptos::prelude::*;
use leptos::task::spawn_local;
use piqueld_client::{ApplyApplicationRequest, Client, DeploymentView, Page, Rollout, Source};

/// "Preview" and "Deploy" buttons for the saved configuration. Preview asks the
/// daemon for a plan (cleared whenever the saved view changes); Deploy starts a
/// deployment of the saved generation, retrying once on transport failure, then
/// switches to the deployments tab.
#[component]
pub(super) fn DeploymentActions() -> impl IntoView {
    let context = editor();
    let preview = RwSignal::new(None::<piqueld_client::PlanView>);
    let deploy = move |_| {
        context.set_error(None);
        let client = match mutation_client() {
            Ok(client) => client,
            Err(error) => {
                context.set_error(Some(error));
                return;
            }
        };
        let app = context.saved.get_untracked();
        let environment = context.environment_id();
        context.busy.set(true);
        spawn_local(async move {
            let mut result = client
                .deploy_environment(&environment, app.generation, None)
                .await;
            if result.as_ref().is_err_and(transport_failure) {
                result = client
                    .deploy_environment(&environment, app.generation, None)
                    .await;
            }
            match result {
                Ok(_) => {
                    context.tab.set("Deployments");
                    context
                        .notice
                        .set("Deployment accepted. Follow its progress below.".into());
                    context.dashboard.with_value(|d| d.refresh.run(()));
                }
                Err(error) => context.failure(&error),
            }
            context.busy.set(false);
        });
    };
    let inspect = move |_| {
        let app = context.saved.get_untracked();
        let request = ApplyApplicationRequest {
            manifest: context.manifest(),
            expected_generation: Some(app.generation),
            expected_application_id: Some(app.application.id().to_string()),
        };
        let environment = context.environment_id();
        context.busy.set(true);
        context.set_error(None);
        spawn_local(async move {
            match Client::browser()
                .plan_application(&request, Some(&environment))
                .await
            {
                Ok(plan) => preview.set(Some(plan)),
                Err(error) => context.set_error(Some(client_error_message(&error))),
            }
            context.busy.set(false);
        });
    };
    Effect::new(move |_| {
        context.saved.track();
        context.environment.track();
        preview.set(None);
    });
    view! {
        <button
            type="button"
            class="btn"
            disabled={move || context.environment_action_blocked()}
            on:click={inspect}
            title="Show what deploying the saved configuration would change"
        >
            {icon(Icon::Eye)}
            "Preview"
        </button>
        <button
            type="button"
            class="btn btn-primary"
            disabled={move || context.environment_action_blocked()}
            on:click={deploy}
        >
            {icon(Icon::Rocket)}
            {move || context.selected_environment().map_or_else(|| "Deploy".into(), |env| format!("Deploy to {}", env.name))}
        </button>
        <DeploymentPreview preview={preview} />
    }
}

/// Panel showing a deployment plan: configuration changes, planned runtime
/// actions and plan diagnostics.
#[component]
fn DeploymentPreview(preview: RwSignal<Option<piqueld_client::PlanView>>) -> impl IntoView {
    let opened = RwSignal::new(false);
    Effect::new(move |_| opened.set(preview.get().is_some()));
    view! {
        <Modal
            title="Deployment preview"
            opened={opened}
            wide=true
            on_close={Callback::new(move |()| preview.set(None))}
        >
            {move || {
                preview
                    .get()
                    .map(|plan| {
                        view! {
                            {plan
                                .identical
                                .then(|| {
                                    notice(
                                        Tone::Info,
                                        "Saved configuration matches the latest deployment snapshot. Deploying refreshes image resolution only.",
                                    )
                                })}
                            <p class="hint">
                                "Image tags and runtime state may change before the deployment runs."
                            </p>
                            <section>
                                <div class="section-header">
                                    <h3>"Configuration changes"</h3>
                                </div>
                                {if plan.changes.is_empty() {
                                    view! {
                                        <p class="hint">
                                            "No configuration changes since the last deployment."
                                        </p>
                                    }
                                        .into_any()
                                } else {
                                    plan.changes
                                        .into_iter()
                                        .map(|change| {
                                            view! {
                                                <div class="diff-row">
                                                    <code>{change.field}</code>
                                                    <span class="before">
                                                        {change.before.unwrap_or_else(|| "absent".into())}
                                                    </span>
                                                    <span class="arrow" aria-hidden="true">
                                                        "→"
                                                    </span>
                                                    <span class="after">
                                                        {change.after.unwrap_or_else(|| "absent".into())}
                                                    </span>
                                                </div>
                                            }
                                        })
                                        .collect_view()
                                        .into_any()
                                }}
                            </section>
                            <section>
                                <div class="section-header">
                                    <h3>"Planned actions"</h3>
                                </div>
                                {if plan.plan.actions.is_empty() {
                                    view! { <p class="hint">"No runtime actions are required."</p> }
                                        .into_any()
                                } else {
                                    view! {
                                        <ul class="stack-sm">
                                            {plan
                                                .plan
                                                .actions
                                                .into_iter()
                                                .map(|action| {
                                                    view! {
                                                        <li class="btn-group">
                                                            {badge(Tone::Neutral, action.kind.name().replace('_', " "))}
                                                            <code>{action.kind.resource_name().to_owned()}</code>
                                                        </li>
                                                    }
                                                })
                                                .collect_view()}
                                        </ul>
                                    }
                                        .into_any()
                                }}
                            </section>
                            <section>
                                <div class="section-header">
                                    <h3>"Rollout"</h3>
                                </div>
                                <ul class="stack-sm">
                                    {plan
                                        .rollouts
                                        .into_iter()
                                        .map(|rollout| {
                                            view! {
                                                <li class="btn-group">
                                                    <code>{rollout.service}</code>
                                                    {badge(Tone::Neutral, rollout.order.as_str())}
                                                    <span class="hint">
                                                        {format!(
                                                            "{} · monitor {}s",
                                                            rollout.order_source.as_str(),
                                                            rollout.monitor_seconds,
                                                        )}
                                                    </span>
                                                </li>
                                            }
                                        })
                                        .collect_view()}
                                </ul>
                            </section>
                            {(!plan.plan.diagnostics.is_empty())
                                .then(|| {
                                    view! {
                                        <div class="stack-sm">
                                            {plan
                                                .plan
                                                .diagnostics
                                                .into_iter()
                                                .map(|d| notice(
                                                    Tone::Warn,
                                                    format!("{}: {}", d.resource, d.message),
                                                ))
                                                .collect_view()}
                                        </div>
                                    }
                                })}
                        }
                    })
            }}
        </Modal>
    }
}

/// Deployment list for the editor's application. The first page is polled while
/// the deployments tab is visible; older pages are appended on request without
/// duplicating deployments already shown.
#[component]
pub(super) fn DeploymentHistory() -> impl IntoView {
    let context = editor();
    let history = RwSignal::new(Vec::<DeploymentView>::new());
    let paginated = RwSignal::new(false);
    let cursor = RwSignal::new(None::<String>);
    let error = RwSignal::new(None::<String>);
    let loading = RwSignal::new(false);
    let id = context.environment_id();
    context.poll_deployments(id.clone(), history, cursor, paginated, error, loading);
    let more = move |_| {
        let id = id.clone();
        let next = cursor.get_untracked();
        loading.set(true);
        spawn_local(async move {
            match Client::browser().deployments(&id, next.as_deref()).await {
                Ok(page) => {
                    error.set(None);
                    paginated.set(true);
                    cursor.set(page.next_cursor);
                    history.update(|items| {
                        for item in page.items {
                            if !items
                                .iter()
                                .any(|old| old.operation.id == item.operation.id)
                            {
                                items.push(item);
                            }
                        }
                    });
                }
                Err(e) => error.set(Some(client_error_message(&e))),
            }
            loading.set(false);
        });
    };
    view! {
        <section class="stack-sm" aria-label="Deployment history">
            {move || error.get().map(|e| notice(Tone::Bad, e))}
            <Show when={move || {
                history.with(Vec::is_empty)
            }}>{empty("No deployments yet. Deploy the saved configuration to create one.")}</Show>
            <For
                each={move || history.get()}
                key={|d| d.operation.id.clone()}
                children={move |initial| {
                    let deployment = Signal::derive(move || {
                        history
                            .with(|items| {
                                items
                                    .iter()
                                    .find(|d| d.operation.id == initial.operation.id)
                                    .cloned()
                                    .unwrap_or_else(|| initial.clone())
                            })
                    });
                    view! { <DeploymentCard deployment={deployment} /> }
                }}
            />
            <Show when={move || cursor.get().is_some()}>
                <div class="btn-group">
                    <button
                        type="button"
                        class="btn"
                        disabled={move || loading.get()}
                        on:click={more.clone()}
                    >
                        "Load older deployments"
                    </button>
                </div>
            </Show>
        </section>
    }
}
/// Applies a freshly polled first page to the history. Before any older page is
/// loaded, the page simply replaces the list and cursor. Afterwards it is merged
/// by operation ID (keeping the older pages), and the `current_target` and
/// `last_successful` flags are cleared on existing entries when the new page
/// carries them, so only one deployment holds each flag.
pub(super) fn merge_history(
    history: RwSignal<Vec<DeploymentView>>,
    cursor: RwSignal<Option<String>>,
    paginated: RwSignal<bool>,
    page: Page<DeploymentView>,
) {
    history.update(|items| {
        if paginated.get_untracked() {
            let has_current = page.items.iter().any(|item| item.current_target);
            let has_successful = page.items.iter().any(|item| item.last_successful);
            for item in items.iter_mut() {
                if has_current {
                    item.current_target = false;
                }
                if has_successful {
                    item.last_successful = false;
                }
            }
            for item in page.items {
                if let Some(old) = items
                    .iter_mut()
                    .find(|v| v.operation.id == item.operation.id)
                {
                    *old = item;
                } else {
                    items.insert(0, item);
                }
            }
            items.sort_by(|a, b| b.operation.id.cmp(&a.operation.id));
        } else {
            *items = page.items;
            cursor.set(page.next_cursor);
        }
    });
}
/// Expandable deployment summary with details, snapshot and attempts tabs.
/// Starts expanded when it matches the `?deployment=` query; attempts load
/// lazily the first time their tab is opened.
#[component]
pub(super) fn DeploymentCard(deployment: Signal<DeploymentView>) -> impl IntoView {
    let initial = deployment.get_untracked();
    let op = initial.operation;
    let selected = leptos_router::hooks::use_query_map()
        .with(|q| q.get("deployment").is_some_and(|id| id == op.id));
    let opened = RwSignal::new(selected);
    let tab = RwSignal::new("Details");
    let attempts_requested = RwSignal::new(false);
    Effect::new(move |_| {
        if opened.get() && tab.get() == "Attempts" {
            attempts_requested.set(true);
        }
    });
    view! {
        <article class="expander" class:selected={selected}>
            <button
                type="button"
                class="expander-summary"
                aria-expanded={move || opened.get().to_string()}
                on:click={move |_| opened.update(|v| *v = !*v)}
            >
                <span class="chevron" aria-hidden="true">
                    {icon(Icon::ChevronRight)}
                </span>
                {move || operation_badge(deployment.get().operation.state)}
                <strong>
                    {move || format!("Deployment #{}", deployment.get().operation.generation)}
                </strong>
                {move || {
                    deployment
                        .get()
                        .current_target
                        .then(|| view! { <span class="tag">"Current target"</span> })
                }}
                {move || {
                    deployment
                        .get()
                        .last_successful
                        .then(|| view! { <span class="tag">"Last successful"</span> })
                }}
                <span class="meta">
                    {move || {
                        let op = deployment.get().operation;
                        view! {
                            {op.phase.map(|phase| format!("{phase} · "))}
                            {when(op.created_at_ms)}
                        }
                    }}
                </span>
            </button>
            <div class="expander-body" hidden={move || !opened.get()}>
                <Tabs
                    label="Deployment sections"
                    options={&["Details", "Snapshot", "Attempts"]}
                    selected={tab}
                />
                <div hidden={move || {
                    tab.get() != "Details"
                }}>
                    {move || {
                        let op = deployment.get().operation;
                        view! {
                            <div class="stack-sm">
                                <dl class="kv">
                                    <dt>"Operation ID"</dt>
                                    <dd>
                                        <code>{op.id}</code>
                                    </dd>
                                    <dt>"Kind"</dt>
                                    <dd>{op.kind.as_str()}</dd>
                                    <dt>"Generation"</dt>
                                    <dd>{op.generation}</dd>
                                    <dt>"Attempts"</dt>
                                    <dd>{op.attempt}</dd>
                                    <dt>"Phase"</dt>
                                    <dd>{op.phase.unwrap_or_else(|| "Waiting".into())}</dd>
                                    {op
                                        .resource
                                        .map(|resource| {
                                            view! {
                                                <dt>"Resource"</dt>
                                                <dd>
                                                    <code>{resource}</code>
                                                </dd>
                                            }
                                        })}
                                    <dt>"Created"</dt>
                                    <dd>{timestamp(op.created_at_ms)}</dd>
                                    <dt>"Updated"</dt>
                                    <dd>{timestamp(op.updated_at_ms)}</dd>
                                    {op
                                        .finished_at_ms
                                        .map(|ms| {
                                            view! {
                                                <dt>"Finished"</dt>
                                                <dd>{timestamp(ms)}</dd>
                                            }
                                        })}
                                </dl>
                                {op
                                    .error_message
                                    .map(|error| {
                                        notice(
                                            Tone::Bad,
                                            view! {
                                                {op
                                                    .error_code
                                                    .map(|code| view! { <strong>{code}</strong> })}
                                                <span>{error}</span>
                                            },
                                        )
                                    })}
                            </div>
                        }
                    }}
                </div>
                <div hidden={move || tab.get() != "Snapshot"}>
                    <DeploymentSnapshot deployment={deployment} />
                </div>
                <div hidden={move || tab.get() != "Attempts"}>
                    <Show when={move || attempts_requested.get()}>
                        <DeploymentAttempts deployment={deployment} />
                    </Show>
                </div>
            </div>
        </article>
    }
}

/// The services, volumes, and jobs captured in a deployment's configuration snapshot.
#[component]
fn DeploymentSnapshot(deployment: Signal<DeploymentView>) -> impl IntoView {
    view! {
        <p class="hint">"Configuration captured when this deployment was accepted."</p>
        {move || {
            let spec = deployment.get().application.to_manifest().spec;
            view! {
                {spec
                    .services
                    .into_iter()
                    .map(|service| view! { <SnapshotService service={service} /> })
                    .collect_view()}
                <div class="snapshot-service">
                    <h4>"Volumes"</h4>
                    <p class="hint">
                        {if spec.volumes.is_empty() {
                            "None".to_owned()
                        } else {
                            spec.volumes.into_iter().map(|v| v.name).collect::<Vec<_>>().join(", ")
                        }}
                    </p>
                </div>
                <div class="snapshot-service">
                    <h4>"Jobs"</h4>
                    {if spec.jobs.is_empty() {
                        view! { <p class="hint">"None"</p> }.into_any()
                    } else {
                        view! {
                            <ol class="hint">
                                {spec
                                    .jobs
                                    .into_iter()
                                    .map(|job| {
                                        view! {
                                            <li>
                                                <strong>{job.name}</strong>
                                                {format!(" on {}, up to {}s: ", job.service, job.timeout_seconds)}
                                                <code>{job.command.join(" ")}</code>
                                            </li>
                                        }
                                    })
                                    .collect_view()}
                            </ol>
                        }
                            .into_any()
                    }}
                </div>
            }
        }}
    }
}

/// Paginated attempt outcomes of one deployment, plus the live attempt while it runs.
#[component]
fn DeploymentAttempts(deployment: Signal<DeploymentView>) -> impl IntoView {
    let op = deployment.get_untracked().operation;
    let attempts = RwSignal::new(Vec::<piqueld_client::Operation>::new());
    let cursor = RwSignal::new(None::<String>);
    let failure = RwSignal::new(None::<String>);
    let loading = RwSignal::new(false);
    let active = Alive::new();
    let load = Callback::new(move |next: Option<String>| {
        if loading.get_untracked() {
            return;
        }
        let environment = op.environment_id.to_string();
        let deployment_id = op.id.clone();
        let active = active.clone();
        loading.set(true);
        spawn_local(async move {
            let result = Client::browser()
                .deployment_attempts(&environment, &deployment_id, next.as_deref())
                .await;
            if !active.get() {
                return;
            }
            match result {
                Ok(page) => {
                    failure.set(None);
                    if next.is_none() {
                        attempts.set(page.items);
                    } else {
                        attempts.update(|items| items.extend(page.items));
                    }
                    cursor.set(page.next_cursor);
                }
                Err(error) => failure.set(Some(client_error_message(&error))),
            }
            loading.set(false);
        });
    });
    load.run(None);
    // The API retains completed outcomes; include the live attempt while it is running.
    let visible_attempts = Signal::derive(move || {
        let mut items = attempts.get();
        let current = deployment.get().operation;
        if current.attempt > 0 && !items.iter().any(|item| item.attempt == current.attempt) {
            items.insert(0, current);
        }
        items
    });

    view! {
        <div class="stack-sm">
            {move || failure.get().map(|error| notice(Tone::Bad, error))}
            {move || loading.get().then(|| empty("Loading attempts…"))}
            {move || {
                if visible_attempts.with(Vec::is_empty) {
                    (!loading.get() && failure.get().is_none())
                        .then(|| empty("No attempts yet."))
                        .into_any()
                } else {
                    view! {
                        <div class="table-wrap">
                            <table class="table">
                                <thead>
                                    <tr>
                                        <th class="num">"Attempt"</th>
                                        <th>"State"</th>
                                        <th>"Outcome"</th>
                                        <th>"Updated"</th>
                                        <th></th>
                                    </tr>
                                </thead>
                                <tbody>
                                    {visible_attempts
                                        .get()
                                        .into_iter()
                                        .map(|attempt| view! { <AttemptRow attempt={attempt} /> })
                                        .collect_view()}
                                </tbody>
                            </table>
                        </div>
                    }
                        .into_any()
                }
            }} <div class="btn-group">
                <button
                    type="button"
                    class="btn btn-sm"
                    disabled={move || loading.get()}
                    on:click={move |_| load.run(None)}
                >
                    {icon(Icon::Refresh)}
                    "Refresh attempts"
                </button>
                <Show when={move || cursor.get().is_some()}>
                    <button
                        type="button"
                        class="btn btn-sm"
                        disabled={move || loading.get()}
                        on:click={move |_| load.run(cursor.get_untracked())}
                    >
                        "Older attempts"
                    </button>
                </Show>
            </div>
        </div>
    }
}

/// One attempt outcome with a link to its events and diagnostics.
#[component]
fn AttemptRow(attempt: piqueld_client::Operation) -> impl IntoView {
    let history = format!("/dashboard/events?operation={}", attempt.id);
    let outcome = match (attempt.error_code, attempt.error_message) {
        (Some(code), Some(message)) => format!("{code}: {message}"),
        (Some(code), None) => code,
        (None, Some(message)) => message,
        (None, None) => String::new(),
    };
    view! {
        <tr>
            <td class="num">{attempt.attempt}</td>
            <td>{operation_badge(attempt.state)}</td>
            <td class="muted">{outcome}</td>
            <td class="muted">{when(attempt.updated_at_ms)}</td>
            <td class="actions">
                <leptos_router::components::A href={history}>"Events"</leptos_router::components::A>
            </td>
        </tr>
    }
}

/// Definition list describing one snapshotted service's configuration.
#[component]
fn SnapshotService(service: piqueld_client::Service) -> impl IntoView {
    let source = match service.source.clone() {
        Source::Image { image } => image,
        Source::Git {
            repository,
            build:
                piqueld_client::Build::Docker {
                    dockerfile,
                    context,
                },
        } => {
            format!(
                "Git: {} · Dockerfile: {} · context: {}",
                repository, dockerfile, context,
            )
        }
    };
    let list = |items: Vec<String>| {
        if items.is_empty() {
            "None".to_owned()
        } else {
            items.join("\n")
        }
    };
    view! {
        <div class="snapshot-service">
            <h4>{service.name.clone()}</h4>
            <dl class="kv">
                <dt>"Source"</dt>
                <dd>
                    <code>{source}</code>
                </dd>
                <dt>"Replicas"</dt>
                <dd>{service.replicas}</dd>
                <dt>"Environment"</dt>
                <dd>
                    {list(service.environment.iter().map(|(k, v)| format!("{k}={v}")).collect())}
                </dd>
                <dt>"Command"</dt>
                <dd>{list(service.command.clone())}</dd>
                <dt>"Arguments"</dt>
                <dd>{list(service.arguments.clone())}</dd>
                <dt>"Mounts"</dt>
                <dd>
                    {list(
                        service
                            .mounts
                            .iter()
                            .map(|m| {
                                format!(
                                    "{} → {}{}",
                                    m.volume,
                                    m.target,
                                    if m.read_only { " (read only)" } else { "" },
                                )
                            })
                            .collect(),
                    )}
                </dd>
                <dt>"Secret files"</dt>
                <dd>
                    {list(
                        service
                            .secrets
                            .iter()
                            .map(|s| format!("{} → {}", s.name, s.target))
                            .collect(),
                    )}
                </dd>
                <dt>"Startup dependencies"</dt>
                <dd>{list(service.depends_on.clone())}</dd>
                <dt>"Rollout"</dt>
                <dd>
                    {format!(
                        "{} · monitor {}s",
                        service.rollout.order.map_or("derived from mounts", |order| order.as_str()),
                        service.rollout.monitor_seconds.unwrap_or(Rollout::DEFAULT_MONITOR_SECONDS),
                    )}
                </dd>
                <SnapshotRuntime service={service} />
            </dl>
        </div>
    }
}

/// Health check and resource limit rows for `SnapshotService`.
#[component]
fn SnapshotRuntime(service: piqueld_client::Service) -> impl IntoView {
    view! {
        <dt>"Health check"</dt>
        <dd>
            {service
                .healthcheck
                .map_or_else(
                    || "None".into(),
                    |check| match check {
                        piqueld_client::HealthCheck::Http {
                            port,
                            path,
                            interval_seconds,
                            timeout_seconds,
                        } => {
                            format!(
                                "HTTP :{port}{path} · every {interval_seconds}s · timeout {timeout_seconds}s",
                            )
                        }
                        piqueld_client::HealthCheck::Command {
                            command,
                            interval_seconds,
                            timeout_seconds,
                        } => {
                            format!(
                                "{} · every {interval_seconds}s · timeout {timeout_seconds}s",
                                command.join(" "),
                            )
                        }
                    },
                )}
        </dd>
        <dt>"Resource limits"</dt>
        <dd>
            {service
                .resources
                .map_or_else(
                    || "Runtime defaults".into(),
                    |r| {
                        format!(
                            "CPU: {} · Memory: {}",
                            r
                                .cpu_millis
                                .map_or_else(|| "default".into(), |v| format!("{v} millicores")),
                            r
                                .memory_bytes
                                .map_or_else(|| "default".into(), |v| format!("{v} bytes")),
                        )
                    },
                )}
        </dd>
    }
}

impl super::EditorContext {
    /// Polls the first deployment page every 2 seconds while the deployments tab is
    /// selected and the page is visible, merging results with `merge_history`.
    /// Stops when the owning view unmounts.
    fn poll_deployments(
        self,
        id: String,
        history: RwSignal<Vec<DeploymentView>>,
        cursor: RwSignal<Option<String>>,
        paginated: RwSignal<bool>,
        error: RwSignal<Option<String>>,
        loading: RwSignal<bool>,
    ) {
        let active = Alive::new();
        let poll_id = id;
        spawn_local(async move {
            while active.get() {
                if self.tab.get_untracked() == "Deployments"
                    && !loading.get_untracked()
                    && !crate::browser::document_hidden()
                {
                    match Client::browser().deployments(&poll_id, None).await {
                        Ok(page) => {
                            if !active.get() {
                                break;
                            }
                            merge_history(history, cursor, paginated, page);
                            error.set(None);
                        }
                        Err(e) => {
                            if active.get() {
                                error.set(Some(client_error_message(&e)));
                            }
                        }
                    }
                }
                gloo_timers::future::TimeoutFuture::new(2000).await;
            }
        });
    }
}
