//! Build metadata and bounded output pages, independent of application runtime logs.
use super::Alive;
use super::format::{bytes, duration, timestamp};
use super::logs::{LogKind, LogViewer, StreamFilter};
use super::ui::{Icon, PageHeader, Tone, build_badge, empty, icon, notice, when};
use super::{client_error_message, dashboard_context, environment_link};
use crate::log_output::LogLine;
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use piqueld_client::{Build, BuildRecord, BuildState, Client, Source};

/// `/builds` page: build history across all applications.
#[component]
pub(super) fn BuildsPage() -> impl IntoView {
    view! {
        <PageHeader
            title="Builds"
            description="Every Git source preparation and job run is recorded, including failed checkouts and cached builds. Image pulls do not create builds."
        />
        <BuildHistory />
    }
}

/// Build list, optionally scoped to one `application`'s environments or to one
/// `environment`. A 3-second loop refetches
/// the first page on demand or while any build is running (skipped while the tab
/// is hidden). Once older pages have been loaded, refreshed builds are merged by
/// ID instead of replacing the list so the extra pages are kept.
#[component]
pub(super) fn BuildHistory(
    #[prop(optional, into)] application: Option<String>,
    #[prop(optional, into)] environment: Option<String>,
) -> impl IntoView {
    let records = RwSignal::new(Vec::<BuildRecord>::new());
    let cursor = RwSignal::new(None::<String>);
    let paginated = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let loading = RwSignal::new(false);
    let refresh = RwSignal::new(true);
    let alive = Alive::new();
    let scoped = environment.is_some();
    let scope = StoredValue::new((application, environment));
    spawn_local(async move {
        while alive.get() {
            let running = records
                .with_untracked(|items| items.iter().any(|b| b.state == BuildState::Running));
            if !super::document_hidden()
                && !loading.get_untracked()
                && (refresh.get_untracked() || running)
            {
                refresh.set(false);
                loading.set(true);
                let (application, environment) = scope.get_value();
                let result = Client::browser()
                    .builds(application.as_deref(), environment.as_deref(), None)
                    .await;
                if !alive.get() {
                    break;
                }
                match result {
                    Ok(page) => {
                        if paginated.get_untracked() {
                            records.update(|items| {
                                for build in page.items {
                                    if let Some(existing) =
                                        items.iter_mut().find(|b| b.id == build.id)
                                    {
                                        *existing = build;
                                    } else {
                                        items.push(build);
                                    }
                                }
                                items.sort_by_key(|b| std::cmp::Reverse(b.id));
                            });
                        } else {
                            records.set(page.items);
                            cursor.set(page.next_cursor);
                        }
                        error.set(None);
                    }
                    Err(e) => error.set(Some(client_error_message(&e))),
                }
                loading.set(false);
            }
            gloo_timers::future::TimeoutFuture::new(3000).await;
        }
    });
    let older = move |_| {
        loading.set(true);
        let (application, environment) = scope.get_value();
        let next = cursor.get_untracked();
        spawn_local(async move {
            match Client::browser()
                .builds(
                    application.as_deref(),
                    environment.as_deref(),
                    next.as_deref(),
                )
                .await
            {
                Ok(page) => {
                    records.update(|items| items.extend(page.items));
                    cursor.set(page.next_cursor);
                    paginated.set(true);
                    error.set(None);
                }
                Err(e) => error.set(Some(client_error_message(&e))),
            }
            loading.set(false);
        });
    };
    view! {
        <section class="stack-sm" aria-label="Build history">
            <div class="toolbar">
                <p class="hint">
                    "Running builds refresh automatically while this page is visible."
                </p>
                <div class="toolbar-end">
                    <button
                        type="button"
                        class="btn btn-sm"
                        disabled={move || loading.get()}
                        on:click={move |_| refresh.set(true)}
                    >
                        {icon(Icon::Refresh)}
                        "Refresh"
                    </button>
                </div>
            </div>
            {move || error.get().map(|e| notice(Tone::Bad, e))}
            <Show when={move || {
                !loading.get() && records.with(Vec::is_empty)
            }}>{empty("No builds recorded yet.")}</Show>
            <For
                each={move || records.get()}
                key={|build| build.id}
                children={move |initial| {
                    let build = Signal::derive(move || {
                        records
                            .with(|items| {
                                items
                                    .iter()
                                    .find(|b| b.id == initial.id)
                                    .cloned()
                                    .unwrap_or_else(|| initial.clone())
                            })
                    });
                    view! { <BuildCard record={build} scoped={scoped} /> }
                }}
            />
            <Show when={move || cursor.get().is_some()}>
                <div class="btn-group">
                    <button
                        type="button"
                        class="btn"
                        disabled={move || loading.get()}
                        on:click={older}
                    >
                        "Load older builds"
                    </button>
                </div>
            </Show>
        </section>
    }
}

/// Expandable summary of one build; expanding shows metadata and mounts `BuildOutput`.
#[component]
fn BuildCard(record: Signal<BuildRecord>, scoped: bool) -> impl IntoView {
    let opened = RwSignal::new(false);
    view! {
        <article class="expander">
            <button
                type="button"
                class="expander-summary"
                aria-expanded={move || opened.get().to_string()}
                on:click={move |_| opened.update(|value| *value = !*value)}
            >
                <span class="chevron" aria-hidden="true">
                    {icon(Icon::ChevronRight)}
                </span>
                {move || build_badge(record.get().state)}
                <strong>
                    {move || {
                        let b = record.get();
                        b.job
                            .map_or_else(
                                || format!("Build #{}", b.id),
                                |job| format!("Job {job} #{}", b.id),
                            )
                    }}
                </strong>
                <span class="tag">{move || record.get().service}</span>
                <span class="meta">{move || build_summary_time(&record.get())}</span>
            </button>
            <div class="expander-body" hidden={move || !opened.get()}>
                {move || {
                    let b = record.get();
                    let duration = build_duration(&b);
                    let environment = b.environment_id.clone();
                    let link = environment_link(dashboard_context().signals, &environment);
                    let operation_id = b.operation_id.clone();
                    view! {
                        <dl class="kv">
                            {(!scoped)
                                .then(|| {
                                    view! {
                                        <dt>"Environment"</dt>
                                        <dd>
                                            {link
                                                .map_or_else(
                                                    || view! { <code>{environment.clone()}</code> }.into_any(),
                                                    |(label, href)| view! { <A href={href}>{label}</A> }.into_any(),
                                                )}
                                        </dd>
                                    }
                                })} <dt>"Operation"</dt> <dd>
                                <A href={format!("/dashboard/events?operation={operation_id}")}>
                                    <code>{operation_id.clone()}</code>
                                </A>
                            </dd>
                            {b
                                .job
                                .map(|job| {
                                    view! {
                                        <dt>"Job"</dt>
                                        <dd>{job}</dd>
                                        <dt>"Exit code"</dt>
                                        <dd>
                                            {b
                                                .exit_code
                                                .map_or_else(|| "None".into(), |code| code.to_string())}
                                        </dd>
                                    }
                                })} <dt>"Started"</dt> <dd>{timestamp(b.started_at_ms)}</dd>
                            <dt>"Finished"</dt>
                            <dd>
                                {b.finished_at_ms.map_or_else(|| "In progress".into(), timestamp)}
                            </dd> <dt>"Duration"</dt> <dd>{duration}</dd> {source_details(b.source)}
                            <dt>"Resolved commit"</dt>
                            <dd>
                                {b
                                    .commit
                                    .map_or_else(
                                        || {
                                            view! { <span class="muted">"Not resolved"</span> }
                                                .into_any()
                                        },
                                        |commit| view! { <code>{commit}</code> }.into_any(),
                                    )}
                            </dd> <dt>"Image"</dt>
                            <dd>
                                {b
                                    .image_id
                                    .map_or_else(
                                        || {
                                            view! { <span class="muted">"Not produced"</span> }
                                                .into_any()
                                        },
                                        |image| view! { <code>{image}</code> }.into_any(),
                                    )}
                            </dd> <dt>"Retained output"</dt>
                            <dd>{bytes(u64::try_from(b.log_bytes).unwrap_or(0))}</dd>
                        </dl>
                    }
                }}
                <Show when={move || opened.get()}>
                    <BuildOutput record={record} />
                </Show>
            </div>
        </article>
    }
}

/// Only mounted for an expanded build; the final successful fetch stops polling.
#[component]
fn BuildOutput(record: Signal<BuildRecord>) -> impl IntoView {
    let chunks = RwSignal::new(Vec::<piqueld_client::BuildLogChunk>::new());
    let prepend_revision = RwSignal::new(0u64);
    let previous = RwSignal::new(None::<i64>);
    let stream = RwSignal::new(None);
    let loading = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let expired = RwSignal::new(false);
    let refresh = RwSignal::new(true);
    let older = RwSignal::new(false);
    Effect::new(move |_| {
        let _ = stream.get();
        refresh.set(true);
    });
    let alive = Alive::new();
    spawn_local(async move {
        let mut elapsed = 30;
        let mut final_loaded = false;
        while alive.get() {
            if !super::document_hidden()
                && !record.get_untracked().log_expired
                && (refresh.get_untracked()
                    || older.get_untracked()
                    || (!final_loaded && elapsed >= 30))
            {
                let replacing = refresh.get_untracked() || !older.get_untracked();
                refresh.set(false);
                older.set(false);
                loading.set(true);
                let filter = stream.get_untracked();
                let before = if replacing {
                    None
                } else {
                    previous.get_untracked()
                };
                let build = record.get_untracked();
                let result = Client::browser().build_logs(build.id, before, filter).await;
                if !alive.get() {
                    break;
                }
                if stream.get_untracked() != filter {
                    refresh.set(true);
                    loading.set(false);
                    continue;
                }
                match result {
                    Ok(page) => {
                        if replacing {
                            chunks.set(page.items);
                            final_loaded = build.state != BuildState::Running;
                        } else if !page.items.is_empty() {
                            batch(move || {
                                prepend_revision
                                    .update(|revision| *revision = revision.wrapping_add(1));
                                chunks.update(|chunks| {
                                    let mut items = page.items;
                                    items.append(chunks);
                                    *chunks = items;
                                });
                            });
                        }
                        previous.set(page.previous_offset);
                        expired.set(page.expired);
                        error.set(None);
                    }
                    Err(e) => error.set(Some(client_error_message(&e))),
                }
                loading.set(false);
                elapsed = 0;
            }
            gloo_timers::future::TimeoutFuture::new(1000).await;
            elapsed += 1;
        }
    });
    let service = record.get_untracked().service;
    let lines = Memo::new(move |_| chunks.with(|chunks| LogLine::build(chunks, &service)));
    view! {
        <div class="section-header">
            <div>
                <h4>"Build output"</h4>
                <p>"Refreshes every 30 seconds while visible until the build finishes."</p>
            </div>
        </div>
        <div class="toolbar">
            <StreamFilter stream={stream} />
            <div class="toolbar-end">
                <Show when={move || previous.get().is_some()}>
                    <button
                        type="button"
                        class="btn btn-sm"
                        disabled={move || loading.get()}
                        on:click={move |_| older.set(true)}
                    >
                        "Load older output"
                    </button>
                </Show>
                <button
                    type="button"
                    class="btn btn-sm"
                    disabled={move || loading.get() || record.get().log_expired}
                    on:click={move |_| refresh.set(true)}
                >
                    {icon(Icon::Refresh)}
                    {move || if loading.get() { "Loading…" } else { "Refresh" }}
                </button>
            </div>
        </div>
        <div class="stack-sm">
            {move || {
                record
                    .get()
                    .log_truncated
                    .then(|| {
                        notice(
                            Tone::Warn,
                            "Output reached the configured byte limit, so its end is not retained.",
                        )
                    })
            }}
            <Show when={move || {
                record.get().log_expired || expired.get()
            }}>
                {notice(
                    Tone::Warn,
                    "Output expired under the retention policy; build metadata remains available.",
                )}
            </Show> {move || error.get().map(|e| notice(Tone::Bad, e))}
        </div>
        <LogViewer
            lines={lines}
            prepend_revision={prepend_revision}
            kind={LogKind::Build}
            label="Build log output"
            empty="No build output was captured."
        />
    }
}

/// Summary line time: duration and start for finished builds, start otherwise.
fn build_summary_time(build: &BuildRecord) -> AnyView {
    match build.finished_at_ms {
        Some(_) => view! {
            {build_duration(build)}
            " · "
            {when(build.started_at_ms)}
        }
        .into_any(),
        None => view! {
            "Started "
            {when(build.started_at_ms)}
        }
        .into_any(),
    }
}

/// Human-readable build duration.
///
/// ```text
/// 850 ms   -> "850 ms"
/// 12340 ms -> "12.3 s"
/// running  -> "In progress"
/// ```
fn build_duration(build: &BuildRecord) -> String {
    build.finished_at_ms.map_or_else(
        || "In progress".into(),
        |finished| duration(finished.saturating_sub(build.started_at_ms)),
    )
}

/// Definition-list rows describing the image or Git source that was built.
fn source_details(source: Source) -> AnyView {
    match source {
        Source::Image { image } => view! {
            <dt>"Source"</dt>
            <dd>
                <code>{image}</code>
            </dd>
        }
        .into_any(),
        Source::Git {
            repository,
            build:
                Build::Docker {
                    dockerfile,
                    context,
                    args,
                    target,
                },
        } => view! {
            <dt>"Repository"</dt>
            <dd>
                <code>{repository.to_string()}</code>
            </dd>
            <dt>"Dockerfile"</dt>
            <dd>
                <code>{dockerfile}</code>
            </dd>
            <dt>"Build context"</dt>
            <dd>
                <code>{context}</code>
            </dd>
            {target
                .map(|target| {
                    view! {
                        <dt>"Build target"</dt>
                        <dd>
                            <code>{target}</code>
                        </dd>
                    }
                })}
            {(!args.is_empty())
                .then(|| {
                    view! {
                        <dt>"Build arguments"</dt>
                        <dd>
                            <code>{build_arguments(&args)}</code>
                        </dd>
                    }
                })}
        }
        .into_any(),
    }
}

/// Renders Docker build arguments as space-separated `KEY="VALUE"` pairs.
/// Values are quoted and escaped so spaces cannot read as extra arguments.
pub(in crate::browser) fn build_arguments(
    args: &std::collections::BTreeMap<String, String>,
) -> String {
    args.iter()
        .map(|(key, value)| format!("{key}={value:?}"))
        .collect::<Vec<_>>()
        .join(" ")
}
