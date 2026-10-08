//! The application's immutable releases, newest first.
use super::super::format::timestamp;
use super::super::ui::{Icon, Tone, empty, icon, notice, when};
use super::client_error_message;
use leptos::prelude::*;
use leptos::task::spawn_local;
use piqueld_client::{Client, ReleaseView, ResolvedSource, ValidatedSource};

/// Releases of `application`, loaded when shown and on refresh. Older pages
/// are appended on request.
#[component]
pub(super) fn ReleaseHistory(application: String) -> impl IntoView {
    let releases = RwSignal::new(Vec::<ReleaseView>::new());
    let cursor = RwSignal::new(None::<String>);
    let error = RwSignal::new(None::<String>);
    let loading = RwSignal::new(false);
    let application = StoredValue::new(application);
    // `None` reloads the first page; a cursor appends the page after it.
    let load = move |next: Option<String>| {
        loading.set(true);
        spawn_local(async move {
            match Client::browser()
                .releases(&application.get_value(), next.as_deref())
                .await
            {
                Ok(page) => {
                    releases.update(|items| {
                        if next.is_none() {
                            items.clear();
                        }
                        items.extend(page.items);
                    });
                    cursor.set(page.next_cursor);
                    error.set(None);
                }
                Err(e) => error.set(Some(client_error_message(&e))),
            }
            loading.set(false);
        });
    };
    load(None);
    view! {
        <section class="stack-sm" aria-label="Releases">
            <div class="toolbar">
                <p class="hint">
                    "Each successful deployment of an environment records an immutable release: its manifest and the exact images it prepared. Environments that prepared the same content share one, and releases outlive the environments that recorded them."
                </p>
                <div class="toolbar-end">
                    <button
                        type="button"
                        class="btn btn-sm"
                        disabled={move || loading.get()}
                        on:click={move |_| load(None)}
                    >
                        {icon(Icon::Refresh)}
                        "Refresh"
                    </button>
                </div>
            </div>
            {move || error.get().map(|e| notice(Tone::Bad, e))}
            <Show when={move || {
                !loading.get() && releases.with(Vec::is_empty)
            }}>{empty("No releases yet. Deploying an environment records one.")}</Show>
            <For
                each={move || releases.get()}
                key={|release| release.id.clone()}
                children={|release| view! { <ReleaseCard release={release} /> }}
            />
            <Show when={move || cursor.get().is_some()}>
                <div class="btn-group">
                    <button
                        type="button"
                        class="btn"
                        disabled={move || loading.get()}
                        on:click={move |_| load(cursor.get_untracked())}
                    >
                        "Load older releases"
                    </button>
                </div>
            </Show>
        </section>
    }
}

/// Expandable release: where its manifest came from, and each service's image,
/// provenance, and build inputs. Starts expanded when it matches `?release=`.
#[component]
fn ReleaseCard(release: ReleaseView) -> impl IntoView {
    let selected = leptos_router::hooks::use_query_map()
        .with(|q| q.get("release").is_some_and(|id| id == release.id.as_str()));
    let opened = RwSignal::new(selected);
    let commit = release.release.commit().map(str::to_owned);
    let services = release
        .release
        .sources()
        .iter()
        .map(|(service, source)| {
            let inputs = release
                .fingerprint
                .services()
                .get(service)
                .map(|inputs| inputs.fields().clone())
                .unwrap_or_default();
            let provenance = match source {
                ResolvedSource::Image { requested, .. } => format!("Pulled {requested}"),
                ResolvedSource::Git {
                    requested: ValidatedSource::Git { repository, .. },
                    commit,
                    ..
                } => format!("Built from {repository} at {commit}"),
                ResolvedSource::Git { commit, .. } => format!("Built at {commit}"),
            };
            view! {
                <div class="snapshot-service">
                    <h4>{service.to_string()}</h4>
                    <dl class="kv">
                        <dt>"Image"</dt>
                        <dd>
                            <code>{source.digest_reference().to_owned()}</code>
                        </dd>
                        <dt>"Provenance"</dt>
                        <dd>{provenance}</dd>
                        {inputs
                            .into_iter()
                            .map(|(field, value)| {
                                view! {
                                    <dt>
                                        <code>{field}</code>
                                    </dt>
                                    <dd>{value}</dd>
                                }
                            })
                            .collect_view()}
                    </dl>
                </div>
            }
        })
        .collect_view();
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
                <strong>
                    <code>{release.id.to_string()}</code>
                </strong>
                <span class="tag">
                    {commit
                        .as_deref()
                        .map_or_else(
                            || "Saved manifest".to_owned(),
                            |commit| format!("Commit {}", &commit[..commit.len().min(12)]),
                        )}
                </span>
                <span class="meta">{when(release.created_at_ms)}</span>
            </button>
            <div class="expander-body" hidden={move || !opened.get()}>
                <dl class="kv">
                    <dt>"Release ID"</dt>
                    <dd>
                        <code>{release.id.to_string()}</code>
                    </dd>
                    <dt>"Recorded"</dt>
                    <dd>{timestamp(release.created_at_ms)}</dd>
                    <dt>"Manifest"</dt>
                    <dd>
                        {commit
                            .map_or_else(
                                || "The saved manifest".to_owned(),
                                |commit| format!("Read at commit {commit}"),
                            )}
                    </dd>
                    <dt>"Content hash"</dt>
                    <dd>
                        <code>{release.content_hash.to_string()}</code>
                    </dd>
                </dl>
                {services}
            </div>
        </article>
    }
}
