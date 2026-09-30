//! Build metadata and bounded output pages, independent of application runtime logs.
use super::client_error_message;
use super::logs::{LogKind, LogViewer, StreamFilter};
use super::management::timestamp;
use crate::log_output::LogLine;
use leptos::*;
use piqueld_client::{Build, BuildRecord, BuildState, Client, Source};
use std::{cell::Cell, rc::Rc};

/// `/builds` page: build history across all applications.
#[component]
pub(super) fn BuildsPage() -> impl IntoView {
    view! {<header class="application-heading"><div><p class="eyebrow">"HISTORY"</p><h2>"Builds"</h2></div></header><BuildHistory/>}
}

/// Build list, optionally scoped to one `application`. A 3-second loop refetches
/// the first page on demand or while any build is running (skipped while the tab
/// is hidden). Once older pages have been loaded, refreshed builds are merged by
/// ID instead of replacing the list so the extra pages are kept.
#[component]
pub(super) fn BuildHistory(#[prop(optional, into)] application: Option<String>) -> impl IntoView {
    let records = create_rw_signal(Vec::<BuildRecord>::new());
    let cursor = create_rw_signal(None::<String>);
    let paginated = create_rw_signal(false);
    let error = create_rw_signal(None::<String>);
    let loading = create_rw_signal(false);
    let refresh = create_rw_signal(true);
    let alive = Rc::new(Cell::new(true));
    let cleanup = alive.clone();
    on_cleanup(move || cleanup.set(false));
    let app = store_value(application);
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
                let application = app.get_value();
                let result = Client::browser().builds(application.as_deref(), None).await;
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
        let application = app.get_value();
        let next = cursor.get_untracked();
        spawn_local(async move {
            match Client::browser()
                .builds(application.as_deref(), next.as_deref())
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
    view! {<section class="deployment-history build-history"><div class="build-history-heading"><p class="help">"Every Git source preparation is recorded, including failed checkouts and cached builds. Image pulls do not create builds."</p>
        <button disabled=move ||loading.get() on:click=move |_|refresh.set(true)>"Refresh builds"</button></div>
        {move ||error.get().map(|e|view!{<p class="form-error" role="alert">{e}</p>})}
        <Show when=move ||!loading.get() && records.with(Vec::is_empty)><p class="empty-state">"No builds recorded yet."</p></Show>
        <For each=move ||records.get() key=|build|build.id children=move |initial|{
            let build=Signal::derive(move ||records.with(|items|items.iter().find(|b|b.id==initial.id).cloned().unwrap_or_else(||initial.clone())));
            view!{<BuildCard record=build/>}
        }/>
        <Show when=move ||cursor.get().is_some()><button disabled=move ||loading.get() on:click=older>"Load older builds"</button></Show>
    </section>}
}

/// Expandable summary of one build; expanding shows metadata and mounts `BuildOutput`.
#[component]
fn BuildCard(record: Signal<BuildRecord>) -> impl IntoView {
    let opened = create_rw_signal(false);
    let toggle = move |_| opened.update(|value| *value = !*value);
    view! {<article class="deployment-card build-card">
        <button class="deployment-summary build-summary" aria-expanded=move ||opened.get().to_string() on:click=toggle>
            {move ||{let b=record.get();let state=build_state(b.state);let summary_time=build_summary_time(&b);view!{
                <span class="deployment-state" data-state=state>{state}</span>
                <strong>{format!("Build #{}",b.id)}</strong>
                <span class="build-service">{b.service}</span>
                <span class="deployment-time">{summary_time}</span>
                <span class="expand-icon" aria-hidden="true">{if opened.get(){"−"}else{"+"}}</span>
            }}}
        </button>
        <div class="deployment-body build-body" hidden=move ||!opened.get()>
            {move ||{let b=record.get();let duration=build_duration(&b);view!{
                <dl class="host-settings build-details">
                    <dt>"Application"</dt>
                    <dd><leptos_router::A href=format!("/dashboard/applications/{}",b.application_id)>{b.application_id}</leptos_router::A></dd>
                    <dt>"Service"</dt><dd>{b.service}</dd>
                    <dt>"Operation ID"</dt><dd><code>{b.operation_id}</code></dd>
                    <dt>"Started"</dt><dd>{timestamp(b.started_at_ms)}</dd>
                    <dt>"Finished"</dt><dd>{b.finished_at_ms.map(timestamp).unwrap_or_else(||"In progress".into())}</dd>
                    <dt>"Duration"</dt><dd>{duration}</dd>
                    {source_details(b.source)}
                    <dt>"Resolved commit"</dt><dd>{b.commit.map(|commit|view!{<code>{commit}</code>}.into_view()).unwrap_or_else(||view!{<span class="help-inline">"Not resolved"</span>}.into_view())}</dd>
                    <dt>"Image"</dt><dd>{b.image_id.map(|image|view!{<code>{image}</code>}.into_view()).unwrap_or_else(||view!{<span class="help-inline">"Not produced"</span>}.into_view())}</dd>
                    <dt>"Retained output"</dt><dd>{format_bytes(b.log_bytes)}</dd>
                </dl>
            }}}
            <Show when=move ||opened.get()><BuildOutput record/></Show>
        </div>
    </article>}
}

/// Paged build log viewer. Only mounted for an expanded build; the final
/// successful fetch stops polling.
///
/// A 1-second loop replaces the newest page on refresh, stream-filter changes,
/// or every 30 seconds while the build runs, and prepends older pages on request
/// (bumping `prepend_revision` so `LogViewer` keeps its scroll position).
/// Responses for a stale stream filter are discarded and refetched.
#[component]
fn BuildOutput(record: Signal<BuildRecord>) -> impl IntoView {
    let chunks = create_rw_signal(Vec::<piqueld_client::BuildLogChunk>::new());
    let prepend_revision = create_rw_signal(0u64);
    let previous = create_rw_signal(None::<i64>);
    let stream = create_rw_signal(None);
    let loading = create_rw_signal(false);
    let error = create_rw_signal(None::<String>);
    let expired = create_rw_signal(false);
    let refresh = create_rw_signal(true);
    let older = create_rw_signal(false);
    create_effect(move |_| {
        let _ = stream.get();
        refresh.set(true);
    });
    let alive = Rc::new(Cell::new(true));
    let cleanup = alive.clone();
    on_cleanup(move || cleanup.set(false));
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
    let lines = create_memo(move |_| chunks.with(|chunks| LogLine::build(chunks, &service)));
    view! {
        <div class="build-output-heading"><h4>"Build output"</h4></div>
        <div class="log-toolbar">
            <StreamFilter stream/>
            <button class="log-refresh" disabled=move ||loading.get() || record.get().log_expired
                on:click=move |_|refresh.set(true)>{move ||if loading.get(){"Loading…"}else{"Refresh output"}}</button>
        </div>
        <p class="help">"Refreshes every 30 seconds while visible until the build finishes."</p>
        {move ||record.get().log_truncated.then(||view!{<p class="build-output-notice">"Output reached the configured byte limit, so its end is not retained."</p>})}
        <Show when=move ||record.get().log_expired || expired.get()><p class="build-output-notice">"Output expired under the retention policy; build metadata remains available."</p></Show>
        {move ||error.get().map(|e|view!{<p class="form-error" role="alert">{e}</p>})}
        <Show when=move ||previous.get().is_some()>
            <button disabled=move ||loading.get() on:click=move |_|older.set(true)>"Load older output"</button>
        </Show>
        <LogViewer lines prepend_revision kind=LogKind::Build label="Build log output" empty="No build output was captured."/>
    }
}

/// Lowercase state label, also used as the `data-state` styling hook.
fn build_state(state: BuildState) -> &'static str {
    match state {
        BuildState::Running => "running",
        BuildState::Succeeded => "succeeded",
        BuildState::Failed => "failed",
        BuildState::Interrupted => "interrupted",
    }
}

/// Summary line time: duration and start for finished builds, start otherwise.
fn build_summary_time(build: &BuildRecord) -> String {
    match build.finished_at_ms {
        Some(_) => format!(
            "{} · {}",
            build_duration(build),
            timestamp(build.started_at_ms)
        ),
        None => format!("Started {}", timestamp(build.started_at_ms)),
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
    let Some(finished) = build.finished_at_ms else {
        return "In progress".into();
    };
    let milliseconds = finished.saturating_sub(build.started_at_ms);
    if milliseconds < 1_000 {
        format!("{milliseconds} ms")
    } else {
        format!("{:.1} s", milliseconds as f64 / 1_000.0)
    }
}

/// Definition-list rows describing the image or Git source that was built.
fn source_details(source: Source) -> View {
    match source {
        Source::Image { image } => view! {<dt>"Source"</dt><dd><code>{image}</code></dd>}.into_view(),
        Source::Git {repository,build:Build::Docker {dockerfile,context}} => view! {
            <dt>"Repository"</dt><dd><code>{repository.url}</code></dd>
            <dt>"Requested revision"</dt><dd><code>{repository.commit.unwrap_or(repository.branch)}</code></dd>
            <dt>"Dockerfile"</dt><dd><code>{dockerfile}</code></dd>
            <dt>"Build context"</dt><dd><code>{context}</code></dd>
        }.into_view(),
    }
}

/// Formats a byte count as `B` below 1 `KiB`, otherwise `KiB` with one decimal.
fn format_bytes(bytes: i64) -> String {
    if bytes < 1_024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KiB", bytes as f64 / 1_024.0)
    }
}
