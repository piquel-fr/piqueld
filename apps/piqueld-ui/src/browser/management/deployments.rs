//! Deployment actions, history, and snapshot inspection.
use super::controls::Tabs;
use super::{client_error_message, editor, mutation_client, transport_failure};
use leptos::{
    Callable, Callback, CollectView, For, IntoView, RwSignal, Show, Signal, SignalGet,
    SignalGetUntracked, SignalSet, SignalUpdate, SignalWith, SignalWithUntracked, component,
    create_effect, create_rw_signal, on_cleanup, spawn_local, view,
};
use piqueld_client::{ApplyApplicationRequest, Client, DeploymentView, Page, Source};
use std::cell::Cell;
use std::rc::Rc;

#[component]
pub(super) fn DeploymentActions() -> impl IntoView {
    let context = editor();
    let preview = create_rw_signal(None::<piqueld_client::PlanView>);
    let deploy = move |_| {
        let Ok(client) = mutation_client() else {
            return;
        };
        let app = context.saved.get_untracked();
        context.busy.set(true);
        context.error.set(None);
        spawn_local(async move {
            let mut result = client
                .deploy_application(app.application.id().as_str(), app.generation)
                .await;
            if result.as_ref().is_err_and(transport_failure) {
                result = client
                    .deploy_application(app.application.id().as_str(), app.generation)
                    .await;
            }
            match result {
                Ok(_) => {
                    context.tab.set("Deployments");
                    context
                        .notice
                        .set("Deployment accepted. Follow its progress below.".into());
                    context.dashboard.with_value(|d| (d.refresh)());
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
        context.busy.set(true);
        context.error.set(None);
        spawn_local(async move {
            match Client::browser().plan_application(&request).await {
                Ok(plan) => preview.set(Some(plan)),
                Err(error) => context.error.set(Some(client_error_message(&error))),
            }
            context.busy.set(false);
        });
    };
    create_effect(move |_| {
        context.saved.track();
        preview.set(None);
    });
    view! {
        <div class="deployment-actions">
            <button disabled={move || context.action_blocked()} on:click={inspect}>
                "Preview"
            </button>
            <button class="primary" disabled={move || context.action_blocked()} on:click={deploy}>
                "Deploy"
            </button>
            <DeploymentPreview preview={preview} />

        </div>
    }
}

#[component]
pub(super) fn DeploymentHistory() -> impl IntoView {
    let context = editor();
    let history = create_rw_signal(Vec::<DeploymentView>::new());
    let paginated = create_rw_signal(false);
    let cursor = create_rw_signal(None::<String>);
    let error = create_rw_signal(None::<String>);
    let loading = create_rw_signal(false);
    let id = context
        .saved
        .with_untracked(|a| a.application.id().to_string());
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
        <section class="deployment-history">
            {move || {
                error
                    .get()
                    .map(|e| {
                        view! {
                            <p class="form-error" role="alert">
                                {e}
                            </p>
                        }
                    })
            }} <Show when={move || history.with(Vec::is_empty)}>
                <p class="empty-state">"No deployments yet."</p>
            </Show>
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
            /> <Show when={move || cursor.get().is_some()}>
                <button disabled={move || loading.get()} on:click={more.clone()}>
                    "Load older deployments"
                </button>
            </Show>
        </section>
    }
}
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
#[component]
pub(super) fn DeploymentCard(deployment: Signal<DeploymentView>) -> impl IntoView {
    let initial = deployment.get_untracked();
    let op = initial.operation;
    let selected = leptos_router::use_query_map().with(|q| q.get("deployment") == Some(&op.id));
    let opened = create_rw_signal(selected);
    let tab = create_rw_signal("Details");
    let attempts_requested = create_rw_signal(false);
    create_effect(move |_| {
        if opened.get() && tab.get() == "Attempts" {
            attempts_requested.set(true);
        }
    });
    view! {
        <article class="deployment-card" class:selected={selected}>
            <button
                class="deployment-summary"
                aria-expanded={move || opened.get().to_string()}
                on:click={move |_| opened.update(|v| *v = !*v)}
            >
                <span
                    class="deployment-state"
                    data-state={move || deployment.get().operation.state.as_str()}
                >
                    {move || deployment.get().operation.state.as_str()}
                </span>
                <strong>
                    {move || format!("Deployment · #{}", deployment.get().operation.generation)}
                </strong>
                {move || {
                    deployment
                        .get()
                        .current_target
                        .then(|| view! { <span class="tag">"Current"</span> })
                }}
                {move || {
                    deployment
                        .get()
                        .last_successful
                        .then(|| view! { <span class="tag">"Last successful"</span> })
                }}
                <span class="deployment-time">
                    {move || timestamp(deployment.get().operation.created_at_ms)}
                </span>
                <span aria-hidden="true">{move || if opened.get() { "−" } else { "+" }}</span>
            </button>
            <div class="deployment-body" hidden={move || !opened.get()}>
                <Tabs
                    label="Deployment sections"
                    options={&["Details", "Snapshot", "Attempts"]}
                    selected={tab}
                    class="tabs"
                />
                <div hidden={move || {
                    tab.get() != "Details"
                }}>
                    {move || {
                        let op = deployment.get().operation;
                        view! {
                            <dl class="host-settings">
                                <dt>"Operation ID"</dt>
                                <dd>
                                    <code>{op.id}</code>
                                </dd>
                                <dt>"Generation"</dt>
                                <dd>{op.generation}</dd>
                                <dt>"Attempt"</dt>
                                <dd>{op.attempt}</dd>
                                <dt>"Phase"</dt>
                                <dd>{op.phase.unwrap_or_else(|| "Waiting".into())}</dd>
                                <dt>"Created"</dt>
                                <dd>{timestamp(op.created_at_ms)}</dd>
                                <dt>"Updated"</dt>
                                <dd>{timestamp(op.updated_at_ms)}</dd>
                            </dl>
                            {op.resource.map(|resource| view! { <p>{resource}</p> })}
                            {op
                                .error_message
                                .map(|error| view! { <p class="form-error">{error}</p> })}
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

#[component]
fn DeploymentSnapshot(deployment: Signal<DeploymentView>) -> impl IntoView {
    view! {
        {move || {
            deployment
                .get()
                .application.to_manifest()
                .spec
                .services
                .into_iter()
                .map(|service| view! { <SnapshotService service={service} /> })
                .collect_view()
        }}
        <p>
            "Volumes: "
            {move || {
                deployment
                    .get()
                    .application.to_manifest()
                    .spec
                    .volumes
                    .into_iter()
                    .map(|v| v.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            }}
        </p>
    }
}

/// Display deployment timestamps in the browser's local timezone.
pub(in crate::browser) fn timestamp(milliseconds: i64) -> String {
    let date = leptos::web_sys::js_sys::Date::new(&leptos::wasm_bindgen::JsValue::from_f64(
        milliseconds
            .to_string()
            .parse::<f64>()
            .expect("integer timestamps are valid floating point numbers"),
    ));
    date.to_locale_string("default", &leptos::web_sys::js_sys::Object::new())
        .into()
}

#[component]
fn DeploymentPreview(preview: RwSignal<Option<piqueld_client::PlanView>>) -> impl IntoView {
    view! {
        {move || {
            preview
                .get()
                .map(|plan| {
                    view! {
                        <div class="preview-panel" role="region" aria-label="Deployment preview">
                            <header>
                                <h3>"Deployment preview"</h3>
                                <button on:click={move |_| {
                                    preview.set(None);
                                }}>"Close preview"</button>
                            </header>
                            <p class="help">
                                "Image tags and runtime state may change before deployment."
                            </p>
                            <ul>
                                {plan
                                    .changes
                                    .into_iter()
                                    .map(|change| {
                                        view! {
                                            <li>
                                                <strong>{change.field}</strong>
                                                " · "
                                                {change.before.unwrap_or_else(|| "absent".into())}
                                                " → "
                                                {change.after.unwrap_or_else(|| "absent".into())}
                                            </li>
                                        }
                                    })
                                    .collect_view()}
                            </ul>
                            <ul>
                                {plan
                                    .plan
                                    .actions
                                    .into_iter()
                                    .map(|action| {
                                        view! {
                                            <li>
                                                {format!(
                                                    "{} · {}",
                                                    action.kind.name(),
                                                    action.kind.resource_name(),
                                                )}
                                            </li>
                                        }
                                    })
                                    .collect_view()}
                            </ul>
                            {plan
                                .plan
                                .diagnostics
                                .into_iter()
                                .map(|d| {
                                    view! {
                                        <p class="form-error">
                                            {format!("{}: {}", d.resource, d.message)}
                                        </p>
                                    }
                                })
                                .collect_view()}
                        </div>
                    }
                })
        }}
    }
}

#[component]
fn DeploymentAttempts(deployment: Signal<DeploymentView>) -> impl IntoView {
    let op = deployment.get_untracked().operation;
    let attempts = create_rw_signal(Vec::<piqueld_client::Operation>::new());
    let cursor = create_rw_signal(None::<String>);
    let failure = create_rw_signal(None::<String>);
    let loading = create_rw_signal(false);
    let active = Rc::new(Cell::new(true));
    let live = Rc::clone(&active);
    on_cleanup(move || live.set(false));
    let load = Callback::new(move |next: Option<String>| {
        if loading.get_untracked() {
            return;
        }
        let app_id = op.application_id.to_string();
        let deployment_id = op.id.clone();
        let active = Rc::clone(&active);
        loading.set(true);
        spawn_local(async move {
            let result = Client::browser()
                .deployment_attempts(&app_id, &deployment_id, next.as_deref())
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
    load.call(None);
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
        {move || {
            failure
                .get()
                .map(|error| {
                    view! {
                        <p class="form-error" role="alert">
                            {error}
                        </p>
                    }
                })
        }}
        {move || {
            visible_attempts
                .get()
                .into_iter()
                .map(|attempt| view! { <AttemptRow attempt={attempt} /> })
                .collect_view()
        }}
        <Show when={move || loading.get()}>
            <p role="status" class="help">
                "Loading attempts…"
            </p>
        </Show>
        <Show when={move || {
            !loading.get() && failure.get().is_none() && visible_attempts.with(Vec::is_empty)
        }}>
            <p class="empty-state">"No attempts yet."</p>
        </Show>
        <div class="form-actions">
            <button disabled={move || loading.get()} on:click={move |_| load.call(None)}>
                "Refresh attempts"
            </button>
            <Show when={move || cursor.get().is_some()}>
                <button
                    disabled={move || loading.get()}
                    on:click={move |_| load.call(cursor.get_untracked())}
                >
                    "Older attempts"
                </button>
            </Show>
        </div>
    }
}

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
                "Git: {} · {} · Dockerfile: {} · context: {}",
                repository.url,
                repository.commit.as_deref().unwrap_or(&repository.branch),
                dockerfile,
                context,
            )
        }
    };
    view! {
        <div class="snapshot-service">
            <h4>{service.name.clone()}</h4>
            <dl>
                <dt>"Source"</dt>
                <dd>{source}</dd>
                <dt>"Replicas"</dt>
                <dd>{service.replicas}</dd>
                <dt>"Environment"</dt>
                <dd>
                    {service
                        .environment
                        .clone()
                        .into_iter()
                        .map(|(k, v)| {
                            view! {
                                <p>
                                    <code>{k}</code>
                                    " = "
                                    {v}
                                </p>
                            }
                        })
                        .collect_view()}
                </dd>
                <dt>"Command"</dt>
                <dd>{service.command.join(" · ")}</dd>
                <dt>"Arguments"</dt>
                <dd>{service.arguments.join(" · ")}</dd>
                <dt>"Mounts"</dt>
                <dd>
                    {service
                        .mounts
                        .clone()
                        .into_iter()
                        .map(|m| {
                            view! {
                                <p>
                                    {format!(
                                        "{} → {}{}",
                                        m.volume,
                                        m.target,
                                        if m.read_only { " (read only)" } else { "" },
                                    )}
                                </p>
                            }
                        })
                        .collect_view()}
                </dd>
                <SnapshotRuntime service={service} />
            </dl>
        </div>
    }
}

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
                                command.join(" · "),
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
    fn poll_deployments(
        self,
        id: String,
        history: RwSignal<Vec<DeploymentView>>,
        cursor: RwSignal<Option<String>>,
        paginated: RwSignal<bool>,
        error: RwSignal<Option<String>>,
        loading: RwSignal<bool>,
    ) {
        let active = Rc::new(Cell::new(true));
        let live = active.clone();
        on_cleanup(move || live.set(false));
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

#[component]
fn AttemptRow(attempt: piqueld_client::Operation) -> impl IntoView {
    let history = format!("/dashboard/events?operation={}", attempt.id);
    view! {
        <p class="attempt">
            {format!(
                "Attempt {} · {} · {} {}",
                attempt.attempt,
                attempt.state,
                attempt.error_code.unwrap_or_default(),
                attempt.error_message.unwrap_or_default(),
            )}
            <leptos_router::A href=history>" View events and diagnostics"</leptos_router::A>
        </p>
    }
}
