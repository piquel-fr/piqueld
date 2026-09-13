//! Build metadata and bounded output pages, independent of application runtime logs.
use super::client_error_message;
use leptos::*;
use piqueld_client::{BuildRecord, BuildState, Client};
use std::{cell::Cell, rc::Rc};

#[component]
pub(super) fn BuildsPage() -> impl IntoView {
    view! {<header class="application-heading"><div><p class="eyebrow">"HISTORY"</p><h2>"Builds"</h2></div></header><BuildHistory/>}
}

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
                && (refresh.get_untracked() || (running && !paginated.get_untracked()))
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
                        records.set(page.items);
                        cursor.set(page.next_cursor);
                        paginated.set(false);
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
    view! {<section class="deployment-history"><p class="help">"Every Git source preparation is recorded, including failed checkouts and cached builds. Image pulls do not create builds."</p>
        <button disabled=move ||loading.get() on:click=move |_|refresh.set(true)>"Refresh builds"</button>
        {move ||error.get().map(|e|view!{<p class="form-error" role="alert">{e}</p>})}
        <Show when=move ||!loading.get() && records.with(Vec::is_empty)><p class="empty-state">"No builds recorded yet."</p></Show>
        <For each=move ||records.get() key=|build|build.id children=move |initial|{
            let build=Signal::derive(move ||records.with(|items|items.iter().find(|b|b.id==initial.id).cloned().unwrap_or_else(||initial.clone())));
            view!{<BuildCard record=build/>}
        }/>
        <Show when=move ||cursor.get().is_some()><button disabled=move ||loading.get() on:click=older>"Load older builds"</button></Show>
    </section>}
}

#[component]
fn BuildCard(record: Signal<BuildRecord>) -> impl IntoView {
    let output = create_rw_signal(None::<piqueld_client::BuildLogPage>);
    let error = create_rw_signal(None::<String>);
    let loading = create_rw_signal(false);
    let offset = create_rw_signal(0i64);
    let load = Callback::new(move |start: i64| {
        loading.set(true);
        offset.set(start);
        let id = record.get_untracked().id;
        spawn_local(async move {
            match Client::browser().build_logs(id, start).await {
                Ok(page) => {
                    output.set(Some(page));
                    error.set(None);
                }
                Err(e) => error.set(Some(client_error_message(&e))),
            }
            loading.set(false);
        });
    });
    view! {<article class="deployment-card">
        {move ||{let b=record.get();let state=match b.state{BuildState::Running=>"running",BuildState::Succeeded=>"succeeded",BuildState::Failed=>"failed",BuildState::Interrupted=>"interrupted"};view!{
            <header><strong>{format!("Build #{} · {}",b.id,b.service)}</strong><span class="deployment-state" data-state=state>{state}</span></header>
            <p><leptos_router::A href=format!("/dashboard/applications/{}",b.application_id)>{b.application_id}</leptos_router::A></p>
            <p class="help">{format!("Started {} · Operation {}",format_time(b.started_at_ms),b.operation_id)}</p>
            {b.finished_at_ms.map(|end|view!{<p>{format!("Duration: {} ms",end.saturating_sub(b.started_at_ms))}</p>})}
            {b.commit.map(|commit|view!{<p>"Commit: "<code>{commit}</code></p>})}
            {b.image_id.map(|image|view!{<p>"Image: "<code>{image}</code></p>})}
            {b.log_truncated.then(||view!{<p class="help">"Output truncated at the configured byte limit."</p>})}
            {b.log_expired.then(||view!{<p class="help">"Output expired; build metadata is retained."</p>})}
        }}}
        <button disabled=move ||loading.get() || record.get().log_expired on:click=move |_|load.call(0)>"View / refresh output"</button>
        {move ||error.get().map(|e|view!{<p class="form-error">{e}</p>})}
        {move ||output.get().map(|page|view!{
            <p class="help">{format!("Output page from byte {}",offset.get())}</p>
            <pre class="build-output">{page.text}</pre>
            {page.expired.then(||view!{<p>"Output has expired."</p>})}
            {page.next_offset.map(|next|view!{<button disabled=move ||loading.get() on:click=move |_|load.call(next)>"Next output page"</button>})}
        })}
    </article>}
}

fn format_time(milliseconds: i64) -> String {
    js_sys::Date::new(&leptos::wasm_bindgen::JsValue::from_f64(
        milliseconds as f64,
    ))
    .to_iso_string()
    .as_string()
    .unwrap_or_else(|| milliseconds.to_string())
}
