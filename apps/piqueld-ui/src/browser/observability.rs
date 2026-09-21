//! Shared diagnostic views, operational history, resource usage and delivery controls.
use super::{client_error_message, management::timestamp};
use leptos::*;
use leptos_router::{A, use_params_map, use_query_map};
use piqueld_client::{
    Client, Event,
    observability::{DaemonStats, EventFilter, EventScope},
};

#[derive(Clone, Copy)]
struct Refresh(RwSignal<u64>);
impl Refresh {
    fn new() -> Self {
        let revision = create_rw_signal(0_u64);
        if let Ok(handle) = set_interval_with_handle(
            move || {
                if !super::document_hidden() {
                    revision.update(|r| *r = r.wrapping_add(1));
                }
            },
            std::time::Duration::from_secs(5),
        ) {
            on_cleanup(move || handle.clear());
        }
        Self(revision)
    }
    fn request(self) {
        self.0.update(|r| *r = r.wrapping_add(1));
    }
    fn since(days: u32) -> Option<i64> {
        if days == 0 {
            return None;
        }
        let now = js_sys::Date::now().to_string().parse::<i64>().ok()?;
        Some(now.saturating_sub(i64::from(days) * 86_400_000))
    }
}
#[component]
pub(super) fn HistoryPage() -> impl IntoView {
    view! {<header class="page-heading"><h1>"Event history"</h1></header><EventHistory/>}
}
#[component]
pub(super) fn ErrorsPage() -> impl IntoView {
    view! {<header class="page-heading"><h1>"Errors"</h1></header><EventHistory errors_only=true/>}
}

#[component]
pub(super) fn EventHistory(
    #[prop(optional, into)] application: Option<String>,
    #[prop(optional)] errors_only: bool,
) -> impl IntoView {
    let refresh = Refresh::new();
    let application = store_value(application);
    let query = use_query_map();
    let cursor = create_rw_signal(None::<String>);
    let kind = create_rw_signal(String::new());
    let code = create_rw_signal(String::new());
    let scope = create_rw_signal(String::new());
    let days = create_rw_signal(30_u32);
    let data = create_local_resource(
        move || {
            (
                refresh.0.get(),
                cursor.get(),
                kind.get(),
                code.get(),
                scope.get(),
                days.get(),
                query.get(),
            )
        },
        move |(_, cursor, kind, code, scope, days, query)| async move {
            let filter = EventFilter {
                application_id: application.get_value(),
                operation_id: query.get("operation").cloned(),
                kind: (!kind.is_empty()).then_some(kind),
                error_code: (!code.is_empty()).then_some(code),
                scope: match scope.as_str() {
                    "daemon" => Some(EventScope::Daemon),
                    "application" => Some(EventScope::Application),
                    _ => None,
                },
                errors_only,
                since_ms: Refresh::since(days),
                descending: true,
                ..EventFilter::default()
            };
            Client::browser()
                .filtered_events(&filter, cursor.as_deref(), 50)
                .await
                .map_err(|e| client_error_message(&e))
        },
    );
    view! {
        <section class="observability-panel">
            <div class="observability-filters">
                <label>"Period"<select on:change=move |e|{days.set(event_target_value(&e).parse().unwrap_or(30));cursor.set(None);}>
                    <option value="1">"24 hours"</option><option value="7">"7 days"</option><option value="30" selected>"30 days"</option><option value="0">"All retained history"</option>
                </select></label>
                <label>"Scope"<select on:change=move |e|{scope.set(event_target_value(&e));cursor.set(None);}>
                    <option value="">"All"</option><option value="application">"Application"</option><option value="daemon">"Daemon"</option>
                </select></label>
                <label>"Event kind"<input placeholder="All kinds" on:change=move |e|{kind.set(event_target_value(&e));cursor.set(None);}/></label>
                <label>"Error code"<input placeholder="All codes" on:change=move |e|{code.set(event_target_value(&e));cursor.set(None);}/></label>
                <button on:click=move |_|{cursor.set(None);refresh.request();}>"Newest"</button>
            </div>
            <p class="help">"Showing retained history. Application deletion removes its history; daemon diagnostics may retain application context."</p>
            {move ||match data.get(){
                None=>view!{<p role="status">"Loading history…"</p>}.into_view(),
                Some(Err(error))=>view!{<p class="form-error" role="alert">{error}</p>}.into_view(),
                Some(Ok(page))=>{
                    let empty=page.items.is_empty();let next=page.next_cursor;
                    view!{<div class="event-list">{empty.then(||view!{<p class="empty-state">"No matching events."</p>})}{page.items.into_iter().map(|event|view!{<EventCard event/>}).collect_view()}</div>
                        {next.map(|next|view!{<button on:click=move |_|cursor.set(Some(next.clone()))>"Older events"</button>})}
                    }.into_view()
                }
            }}
        </section>
    }
}
#[component]
fn EventCard(event: Event) -> impl IntoView {
    let summary = event
        .message
        .clone()
        .unwrap_or_else(|| event.kind.replace('_', " "));
    let context = format!(
        "{} · {} · {}{}",
        timestamp(event.created_at_ms),
        event.scope.as_str(),
        event.phase.as_deref().unwrap_or("control plane"),
        event
            .resource
            .as_ref()
            .map_or_else(String::new, |r| format!(" · {r}"))
    );
    let diagnostic = event.diagnostic.as_ref().map(|d| d.id.clone());
    view! {<article class="event-card"><p class="help">{context}</p><strong>{summary}</strong>
        <p><code>{event.kind}</code>{event.attempt.map(|attempt|format!(" · attempt {attempt}"))}{event.retry.map(|retry|format!(" · request {retry}"))}{event.duration_ms.map(|ms|format!(" · {ms} ms"))}</p>
        {event.application_id.map(|id|view!{<p class="help">"Application: "<code>{id.to_string()}</code></p>})}
        {event.action_id.map(|id|view!{<p class="help">"Action: "<code>{id}</code></p>})}
        {event.operation_id.map(|id|view!{<p><A href=format!("/dashboard/events?operation={id}")>"Related operation events"</A></p>})}
        {diagnostic.map(|id|view!{<A href=format!("/dashboard/errors/{id}")>"Details"</A>})}
    </article>}
}
#[component]
pub(super) fn DiagnosticPage() -> impl IntoView {
    let params = use_params_map();
    let data = create_local_resource(
        move || params.with(|p| p.get("id").cloned().unwrap_or_default()),
        |id| async move {
            Client::browser()
                .diagnostic(&id)
                .await
                .map_err(|e| client_error_message(&e))
        },
    );
    view! {<header class="page-heading"><h1>"Diagnostic details"</h1><A href="/dashboard/errors">"All errors"</A></header>
        {move ||match data.get(){None=>view!{<p>"Loading diagnostic…"</p>}.into_view(),Some(Err(e))=>view!{<p class="form-error">{e}</p>}.into_view(),Some(Ok(event))=>view!{<DiagnosticDetails event/>}.into_view()}}
    }
}
#[component]
fn DiagnosticDetails(event: Event) -> impl IntoView {
    let detail = event.diagnostic.clone();
    let operation = event.operation_id.clone();
    view! {<section class="observability-panel"><EventCard event=event.clone()/>
        <dl class="observability-values">
            <dt>"Application"</dt><dd>{event.application_id.map_or_else(||"Daemon".into(),|id|id.to_string())}</dd>
            <dt>"Operation"</dt><dd>{operation.clone().unwrap_or_else(||"None".into())}</dd>
            <dt>"Action"</dt><dd>{event.action_id.unwrap_or_else(||"None".into())}</dd>
            <dt>"Request"</dt><dd>{event.request_id.unwrap_or_else(||"None".into())}</dd>
        </dl>
        {detail.map(|d|view!{<h2>{d.code}</h2><p>{d.summary}</p><ul>{d.causes.into_iter().map(|cause|view!{<li>{cause}</li>}).collect_view()}</ul><p><strong>"Next action: "</strong>{d.next_action}</p><p>{if d.retryable{"Automatic recovery is supported; inspect related events for the latest outcome."}else{"Administrator intervention may be required."}}</p><p class="help">{format!("Diagnostic ID: {}",d.id)}</p>})}
        {operation.map(|id|view!{<A href=format!("/dashboard/events?operation={id}")>"Related operation events"</A>})}
    </section>}
}
#[component]
pub(super) fn SystemPage() -> impl IntoView {
    let refresh = Refresh::new();
    let data = create_local_resource(
        move || refresh.0.get(),
        |_| async {
            Client::browser()
                .daemon_stats()
                .await
                .map_err(|e| client_error_message(&e))
        },
    );
    view! {<header class="page-heading"><h1>"Daemon status"</h1><button on:click=move |_|refresh.request()>"Refresh"</button></header>
        <super::dashboard::ReadinessPanel/>
        {move ||match data.get(){None=>view!{<p>"Collecting resource usage…"</p>}.into_view(),Some(Err(e))=>view!{<p class="form-error">{e}</p>}.into_view(),Some(Ok(stats))=>view!{<ResourceStats stats/>}.into_view()}}
    }
}
#[component]
fn ResourceStats(stats: DaemonStats) -> impl IntoView {
    let bytes = |v: Option<u64>| {
        v.map_or_else(
            || "Unavailable".into(),
            |v| format!("{v} bytes ({} MiB)", v / 1_048_576),
        )
    };
    let fields = [
        ("Uptime", format!("{} seconds", stats.uptime_seconds)),
        ("Memory", bytes(stats.memory_bytes)),
        (
            "CPU",
            stats.cpu_percent.map_or_else(
                || "Collecting / unavailable".into(),
                |v| format!("{v:.1}% (100% = one core)"),
            ),
        ),
        ("Database", bytes(Some(stats.database_bytes))),
        ("WAL", bytes(Some(stats.wal_bytes))),
        ("Available disk", bytes(stats.available_disk_bytes)),
        ("Events", stats.events.to_string()),
        ("Diagnostic occurrences", stats.diagnostics.to_string()),
        (
            "Build output",
            format!("{} bytes", stats.build_output_bytes),
        ),
        ("Running operations", stats.running_operations.to_string()),
        ("Queued operations", stats.queued_operations.to_string()),
        ("Pending deliveries", stats.pending_deliveries.to_string()),
        ("Failed deliveries", stats.failed_deliveries.to_string()),
    ];
    view! {<section class="observability-panel"><p class="help">{format!("Measured {}. Historical resource graphs require an external metrics collector.",timestamp(stats.sampled_at_ms))}</p><dl class="observability-values">{fields.into_iter().map(|(label,value)|view!{<dt>{label}</dt><dd>{value}</dd>}).collect_view()}</dl></section>}
}
#[component]
pub(super) fn AnalyticsPage() -> impl IntoView {
    let refresh = Refresh::new();
    let days = create_rw_signal(30_u32);
    let data = create_local_resource(
        move || (refresh.0.get(), days.get()),
        |(_, days)| async move {
            Client::browser()
                .deployment_analytics(None, Refresh::since(days), None)
                .await
                .map_err(|e| client_error_message(&e))
        },
    );
    view! {<header class="page-heading"><h1>"Deployment analytics"</h1><select aria-label="Analytics period" on:change=move |e|days.set(event_target_value(&e).parse().unwrap_or(30))><option value="1">"24 hours"</option><option value="7">"7 days"</option><option value="30" selected>"30 days"</option><option value="365">"365 days"</option></select></header>
        {move ||match data.get(){None=>view!{<p>"Loading analytics…"</p>}.into_view(),Some(Err(e))=>view!{<p class="form-error">{e}</p>}.into_view(),Some(Ok(a))=>view!{<section class="observability-panel">
            {a.incomplete.then(||view!{<p class="conflict-notice">"This interval includes unavailable detailed history. Counts describe retained applications and records only."</p>})}
            <p>{format!("{} deployments · {} succeeded · {} failed at their last attempt in this interval",a.deployments,a.succeeded,a.failed)}</p>
            <p>{format!("{} failed attempts · {} retry attempts · {} action retries",a.failed_attempts,a.retry_attempts,a.action_retries)}</p>
            <p>{a.mean_duration_ms.map_or_else(||"No measured deployment durations".into(),|v|format!("Mean deployment attempt: {:.2} seconds",v/1000.0))}</p>
            <h2>"Action durations"</h2><ul>{a.actions.into_iter().map(|r|view!{<li>{format!("{}: {:.2} seconds mean across {} actions",r.phase,r.mean_ms/1000.0,r.count)}</li>}).collect_view()}</ul>
            <h2>"Common failures"</h2><ul>{a.failures.into_iter().map(|r|view!{<li><code>{r.code}</code>{format!(": {} occurrences",r.count)}</li>}).collect_view()}</ul>
        </section>}.into_view()}}
    }
}
#[component]
pub(super) fn NotificationsPage() -> impl IntoView {
    let refresh = Refresh::new();
    let cursor = create_rw_signal(None::<String>);
    let error = create_rw_signal(None::<String>);
    let data = create_local_resource(
        move || (refresh.0.get(), cursor.get()),
        |(_, cursor)| async move {
            Client::browser()
                .notification_deliveries(cursor.as_deref())
                .await
                .map_err(|e| client_error_message(&e))
        },
    );
    view! {<header class="page-heading"><h1>"Notification deliveries"</h1><button on:click=move |_|{cursor.set(None);refresh.request();}>"Newest"</button></header><p class="help">"Destinations and notification categories are configured in daemon TOML. Delivery may be repeated after a lost acknowledgement."</p>
        {move ||error.get().map(|e|view!{<p class="form-error">{e}</p>})}
        {move ||match data.get(){None=>view!{<p>"Loading deliveries…"</p>}.into_view(),Some(Err(e))=>view!{<p class="form-error">{e}</p>}.into_view(),Some(Ok(page))=>view!{<section class="observability-panel">
            {page.items.is_empty().then(||view!{<p>"No notification deliveries."</p>})}
            {page.items.into_iter().map(|d|{let id=d.id.clone();view!{<article class="event-card"><strong>{format!("{} · {} · {}",d.destination,d.category,d.state)}</strong><p>{format!("{} attempts · {}",d.attempts,timestamp(d.updated_at_ms))}</p><p>{d.last_error}</p><p class="help">{d.id}</p>{(d.state=="failed").then(||view!{<button on:click=move |_|{let id=id.clone();spawn_local(async move{match Client::browser().retry_notification(&id).await {Ok(())=>{error.set(None);refresh.request();},Err(e)=>error.set(Some(client_error_message(&e)))}});}>"Retry delivery"</button>})}</article>}}).collect_view()}
            {page.next_cursor.map(|next|view!{<button on:click=move |_|cursor.set(Some(next.clone()))>"Older deliveries"</button>})}
        </section>}.into_view()}}
    }
}
