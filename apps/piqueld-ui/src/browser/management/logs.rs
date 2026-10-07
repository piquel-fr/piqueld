//! Recent container output for one application or one of its services.
use super::super::ui::{Icon, Tone, icon, notice};
use super::{EditorContext, client_error_message};
use crate::browser::Alive;
use crate::{
    browser::logs::{LogKind, LogViewer, StreamFilter},
    log_output::LogLine,
};
use leptos::prelude::*;
use leptos::task::spawn_local;
use piqueld_client::Client;

/// Runtime log card for the editor's application: the latest 200 lines from the
/// last hour, refetched every 30 seconds while visible or when the service or
/// stream filter changes. The service selector lists the services of the
/// manifest the shown environment deploys; `fixed_service` scopes it to one
/// service and hides the selector. Responses for an outdated filter are discarded and refetched.
#[component]
pub(super) fn ApplicationLogs(#[prop(optional)] fixed_service: Option<String>) -> impl IntoView {
    let context = use_context::<EditorContext>().expect("application editor");
    let logs = RwSignal::new(None::<piqueld_client::ApplicationLogs>);
    let error = RwSignal::new(None::<String>);
    let loading = RwSignal::new(false);
    let scoped = fixed_service.is_some();
    let service = RwSignal::new(fixed_service.unwrap_or_default());
    let stream = RwSignal::new(None);
    let refresh = RwSignal::new(true);
    Effect::new(move |_| {
        let _ = (service.get(), stream.get());
        refresh.set(true);
    });
    let alive = Alive::new();
    let id = context.environment_id();
    spawn_local(async move {
        let mut elapsed = 30;
        while alive.get() {
            if !super::super::document_hidden() && (refresh.get_untracked() || elapsed >= 30) {
                refresh.set(false);
                loading.set(true);
                let filter = (service.get_untracked(), stream.get_untracked());
                let result = Client::browser()
                    .filtered_environment_logs(
                        &id,
                        (!filter.0.is_empty()).then_some(filter.0.as_str()),
                        200,
                        3600,
                        filter.1,
                    )
                    .await;
                if !alive.get() {
                    break;
                }
                if (service.get_untracked(), stream.get_untracked()) != filter {
                    refresh.set(true);
                    loading.set(false);
                    continue;
                }
                match result {
                    Ok(value) => {
                        logs.set(Some(value));
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
    let lines = Memo::new(move |_| {
        logs.get()
            .map(|logs| LogLine::runtime(logs.items))
            .unwrap_or_default()
    });
    view! {
        <section class="card">
            <header>
                <div>
                    <h3>{if scoped { "Service logs" } else { "Application logs" }}</h3>
                    <p>
                        "Latest 200 lines from the last hour, read directly from Docker. Refreshes every 30 seconds while visible."
                    </p>
                </div>
                <button
                    type="button"
                    class="btn btn-sm"
                    disabled={move || loading.get()}
                    on:click={move |_| refresh.set(true)}
                >
                    {icon(Icon::Refresh)}
                    {move || if loading.get() { "Loading…" } else { "Refresh" }}
                </button>
            </header>
            <div class="toolbar">
                <Show when={move || !scoped}>
                    <label class="field">
                        <span>"Service"</span>
                        <select
                            prop:value={move || service.get()}
                            on:change={move |e| service.set(event_target_value(&e))}
                        >
                            <option value="">"All services"</option>
                            {move || {
                                context
                                    .environment_manifest()
                                    .map_or_else(|| context.manifest(), |manifest| manifest.to_manifest())
                                    .spec
                                    .services
                                    .into_iter()
                                    .map(|s| {
                                        view! {
                                            <option value={s.name.clone()}>{s.name.clone()}</option>
                                        }
                                    })
                                    .collect_view()
                            }}
                        </select>
                    </label>
                </Show>
                <StreamFilter stream={stream} />
            </div>
            <div class="stack-sm">
                <Show when={move || stream.get().is_some()}>
                    <p class="hint">
                        "Merged terminal output is only included when both streams are selected."
                    </p>
                </Show>
                {move || error.get().map(|e| notice(Tone::Bad, e))}
                <Show when={move || {
                    logs.with(|logs| logs.as_ref().is_some_and(|logs| logs.truncated))
                }}>
                    {notice(
                        Tone::Warn,
                        "Snapshot truncated. Filter by service or stream to narrow the output.",
                    )}
                </Show>
            </div>
            <LogViewer
                lines={lines}
                label="Application log output"
                empty="No recent output available."
                kind={if scoped { LogKind::Service } else { LogKind::Application }}
            />
        </section>
    }
}
