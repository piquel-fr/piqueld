use super::{EditorContext, client_error_message};
use leptos::*;
use piqueld_client::Client;
use std::{cell::Cell, rc::Rc};

#[component]
pub(super) fn ApplicationLogs() -> impl IntoView {
    let context = use_context::<EditorContext>().expect("application editor");
    let logs = create_rw_signal(None::<piqueld_client::ApplicationLogs>);
    let error = create_rw_signal(None::<String>);
    let loading = create_rw_signal(false);
    let service = create_rw_signal(String::new());
    let polling = create_rw_signal(false);
    let refresh = create_rw_signal(true);
    let alive = Rc::new(Cell::new(true));
    let cleanup = alive.clone();
    on_cleanup(move || cleanup.set(false));
    let id = context
        .saved
        .with_untracked(|a| a.application.id.to_string());
    spawn_local(async move {
        let mut elapsed = 5;
        while alive.get() {
            if !super::super::document_hidden()
                && (refresh.get_untracked() || (polling.get_untracked() && elapsed >= 5))
            {
                refresh.set(false);
                loading.set(true);
                let filter = service.get_untracked();
                let result = Client::browser()
                    .application_logs(
                        &id,
                        (!filter.is_empty()).then_some(filter.as_str()),
                        200,
                        3600,
                    )
                    .await;
                if !alive.get() {
                    break;
                }
                match result {
                    Ok(value) => {
                        logs.set(Some(value));
                        error.set(None);
                    }
                    Err(e) => {
                        error.set(Some(client_error_message(&e)));
                    }
                }
                loading.set(false);
                elapsed = 0;
            }
            gloo_timers::future::TimeoutFuture::new(1000).await;
            elapsed += 1;
        }
    });
    view! {<section class="settings-card"><h3>"Application logs"</h3>
        <p class="help">"Recent output from Docker: up to 200 lines from the last hour. Refresh replaces this snapshot."</p>
        <label>"Service (empty for all)"<input prop:value=move ||service.get() on:input=move |e|service.set(event_target_value(&e)) /></label>
        <button disabled=move ||loading.get() on:click=move |_|refresh.set(true)>"Refresh logs"</button>
        <label><input type="checkbox" prop:checked=move ||polling.get() on:change=move |e|polling.set(event_target_checked(&e))/>"Refresh every 5 seconds while visible"</label>
        {move ||error.get().map(|e|view!{<p class="form-error" role="alert">{e}</p>})}
        {move ||logs.get().map(|logs|view!{
            {logs.truncated.then(||view!{<p class="help">"Snapshot truncated. Filter by service to narrow the output."</p>})}
            <pre class="application-logs" tabindex="0" role="region" aria-label="Application log output">{if logs.items.is_empty(){"No recent output available.".into()}else{logs.items.into_iter().map(|line|format!("{} {} {} {} | {}",line.timestamp,line.service,line.task_id,line.stream,line.message)).collect::<Vec<_>>().join("\n")}}</pre>
        })}
    </section>}
}
