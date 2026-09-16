use super::{EditorContext, client_error_message};
use crate::{
    browser::logs::{LogViewer, StreamFilter},
    log_output::LogLine,
};
use leptos::*;
use piqueld_client::Client;
use std::{cell::Cell, rc::Rc};

#[component]
pub(super) fn ApplicationLogs(#[prop(optional)] fixed_service: Option<String>) -> impl IntoView {
    let context = use_context::<EditorContext>().expect("application editor");
    let logs = create_rw_signal(None::<piqueld_client::ApplicationLogs>);
    let error = create_rw_signal(None::<String>);
    let loading = create_rw_signal(false);
    let scoped = fixed_service.is_some();
    let service = create_rw_signal(fixed_service.unwrap_or_default());
    let stream = create_rw_signal(None);
    let refresh = create_rw_signal(true);
    create_effect(move |_| {
        let _ = (service.get(), stream.get());
        refresh.set(true);
    });
    let alive = Rc::new(Cell::new(true));
    let cleanup = alive.clone();
    on_cleanup(move || cleanup.set(false));
    let id = context
        .saved
        .with_untracked(|a| a.application.id().to_string());
    spawn_local(async move {
        let mut elapsed = 30;
        while alive.get() {
            if !super::super::document_hidden() && (refresh.get_untracked() || elapsed >= 30) {
                refresh.set(false);
                loading.set(true);
                let filter = (service.get_untracked(), stream.get_untracked());
                let result = Client::browser()
                    .filtered_application_logs(
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
    let lines = Signal::derive(move || {
        logs.get()
            .map(|logs| LogLine::runtime(logs.items))
            .unwrap_or_default()
    });
    view! {<section class="settings-card log-card"><h3>{if scoped {"Service logs"} else {"Application logs"}}</h3>
        <p class="help">"Latest 200 lines from the last hour. Refreshes every 30 seconds while visible."</p>
        <div class="log-toolbar">
            <Show when=move ||!scoped>
                <label class="log-filter">"Service"<select prop:value=move ||service.get() on:change=move |e|service.set(event_target_value(&e))>
                    <option value="">"All services"</option>
                    {move ||context.saved.with(|app| app.application.to_manifest().spec.services.into_iter().map(|s| view!{<option value=s.name.clone()>{s.name}</option>}).collect_view())}
                </select></label>
            </Show>
            <StreamFilter stream/>
            <button class="log-refresh" disabled=move ||loading.get() on:click=move |_|refresh.set(true)>{move ||if loading.get(){"Loading…"}else{"Refresh logs"}}</button>
        </div>
        <Show when=move ||stream.get().is_some()><p class="help">"Merged terminal output is only included when Both streams are selected."</p></Show>
        {move ||error.get().map(|e|view!{<p class="form-error" role="alert">{e}</p>})}
        <Show when=move ||logs.with(|logs|logs.as_ref().is_some_and(|logs|logs.truncated))><p class="help">"Snapshot truncated. Filter by service or stream to narrow the output."</p></Show>
        <LogViewer lines label="Application log output" empty="No recent output available." scoped/>
    </section>}
}
