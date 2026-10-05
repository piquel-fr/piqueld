//! Event history, diagnostics, daemon statistics, analytics, and notification deliveries.
use super::format::{bytes, duration, duration_f64, duration_secs, now_ms, timestamp};
use super::ui::{Icon, PageHeader, Tone, badge, empty, icon, metric, notice, when};
use super::{client_error_message, dashboard_context, environment_link};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::{use_params_map, use_query_map};
use piqueld_client::{
    Client, Event,
    observability::{DaemonStats, DeliveryState, EventFilter, EventScope},
};

/// Revision counter that observability resources track to refetch.
/// Bumped every 5 seconds while the tab is visible, or on demand.
#[derive(Clone, Copy)]
struct Refresh(RwSignal<u64>);
impl Refresh {
    /// Starts the 5-second interval, cleared when the owning view unmounts.
    fn new() -> Self {
        let revision = RwSignal::new(0_u64);
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
    /// Triggers an immediate refetch.
    fn request(self) {
        self.0.update(|r| *r = r.wrapping_add(1));
    }
    /// Unix milliseconds `days` ago, or `None` (no lower bound) for `0`.
    fn since(days: u32) -> Option<i64> {
        if days == 0 {
            return None;
        }
        Some(now_ms().saturating_sub(i64::from(days) * 86_400_000))
    }
}

/// Period picker; `changed` runs after the selection so lists can restart at the newest page.
fn period_select(days: RwSignal<u32>, all: bool, changed: impl Fn() + 'static) -> AnyView {
    view! {
        <label class="field">
            <span>"Period"</span>
            <select on:change={move |e| {
                days.set(event_target_value(&e).parse().unwrap_or(30));
                changed();
            }}>
                <option value="1">"24 hours"</option>
                <option value="7">"7 days"</option>
                <option value="30" selected>
                    "30 days"
                </option>
                {if all {
                    view! { <option value="0">"All retained history"</option> }
                } else {
                    view! { <option value="365">"365 days"</option> }
                }}
            </select>
        </label>
    }
    .into_any()
}

/// `/events` page: all retained events.
#[component]
pub(super) fn HistoryPage() -> impl IntoView {
    view! {
        <PageHeader
            title="Events"
            description="Retained control-plane history across every application and the daemon itself."
        />
        <EventHistory />
    }
}

/// `/errors` page: events that carry an error.
#[component]
pub(super) fn ErrorsPage() -> impl IntoView {
    view! {
        <PageHeader
            title="Errors"
            description="Failures recorded by the daemon, with diagnostics explaining causes and next steps."
        />
        <EventHistory errors_only=true />
    }
}

/// Filterable, paginated event list (50 per page), optionally scoped to one
/// application (with all its environments) or one environment. Also honours an `?operation=` query filter; changing any filter
/// or the operation resets to the newest page. Auto-refreshes via `Refresh`.
#[component]
pub(super) fn EventHistory(
    #[prop(optional, into)] application: Option<String>,
    #[prop(optional, into)] environment: Option<String>,
    #[prop(optional)] errors_only: bool,
) -> impl IntoView {
    let refresh = Refresh::new();
    let scoped = environment.is_some();
    let owned = scoped || application.is_some();
    let scope_filter = StoredValue::new((application, environment));
    let query = use_query_map();
    let cursor = RwSignal::new(None::<String>);
    Effect::new(move |previous: Option<Option<String>>| {
        let operation = query.with(|query| query.get("operation"));
        if previous.as_ref() != Some(&operation) {
            cursor.set(None);
        }
        operation
    });
    let kind = RwSignal::new(String::new());
    let code = RwSignal::new(String::new());
    let scope = RwSignal::new(String::new());
    let days = RwSignal::new(30_u32);
    let failures = RwSignal::new(errors_only);
    let data = LocalResource::new(move || {
        refresh.0.track();
        let (cursor, kind, code, scope, days, failures, query) = (
            cursor.get(),
            kind.get(),
            code.get(),
            scope.get(),
            days.get(),
            failures.get(),
            query.get(),
        );
        async move {
            let (application_id, environment_id) = scope_filter.get_value();
            let filter = EventFilter {
                application_id,
                environment_id,
                operation_id: query.get("operation"),
                kind: (!kind.is_empty()).then_some(kind),
                error_code: (!code.is_empty()).then_some(code),
                scope: match scope.as_str() {
                    "daemon" => Some(EventScope::Daemon),
                    "application" => Some(EventScope::Application),
                    _ => None,
                },
                errors_only: failures,
                since_ms: Refresh::since(days),
                descending: true,
                ..EventFilter::default()
            };
            Client::browser()
                .filtered_events(&filter, cursor.as_deref(), 50)
                .await
                .map_err(|e| client_error_message(&e))
        }
    });
    let reset = move || cursor.set(None);
    view! {
        <div class="toolbar">
            {period_select(days, true, reset)} <Show when={move || !owned}>
                <label class="field">
                    <span>"Scope"</span>
                    <select on:change={move |e| {
                        scope.set(event_target_value(&e));
                        reset();
                    }}>
                        <option value="">"All"</option>
                        <option value="application">"Application"</option>
                        <option value="daemon">"Daemon"</option>
                    </select>
                </label>
            </Show> <label class="field">
                <span>"Event kind"</span>
                <input
                    placeholder="All kinds"
                    on:change={move |e| {
                        kind.set(event_target_value(&e));
                        reset();
                    }}
                />
            </label> <label class="field">
                <span>"Error code"</span>
                <input
                    placeholder="All codes"
                    on:change={move |e| {
                        code.set(event_target_value(&e));
                        reset();
                    }}
                />
            </label> <Show when={move || !errors_only}>
                <label class="checkbox" style="padding-bottom:8px">
                    <input
                        type="checkbox"
                        prop:checked={move || failures.get()}
                        on:change={move |e| {
                            failures.set(event_target_checked(&e));
                            reset();
                        }}
                    />
                    "Errors only"
                </label>
            </Show>
            <div class="toolbar-end">
                {move || {
                    query
                        .with(|q| q.get("operation"))
                        .map(|operation| {
                            view! {
                                <span class="tag" title={operation}>
                                    "Filtered by operation"
                                </span>
                            }
                        })
                }}
                <button
                    type="button"
                    class="btn"
                    on:click={move |_| {
                        reset();
                        refresh.request();
                    }}
                >
                    {icon(Icon::Refresh)}
                    "Newest"
                </button>
            </div>
        </div>
        {move || match data.get() {
            None => empty("Loading history…"),
            Some(Err(error)) => notice(Tone::Bad, error),
            Some(Ok(page)) => {
                let next = page.next_cursor;
                view! {
                    <div class="list">
                        {if page.items.is_empty() {
                            empty("No matching events.")
                        } else {
                            page.items
                                .into_iter()
                                .map(|event| view! { <EventCard event={event} scoped={scoped} /> })
                                .collect_view()
                                .into_any()
                        }}
                    </div>
                    {next
                        .map(|next| {
                            view! {
                                <div class="btn-group" style="margin-top:12px">
                                    <button
                                        type="button"
                                        class="btn"
                                        on:click={move |_| cursor.set(Some(next.clone()))}
                                    >
                                        "Older events"
                                    </button>
                                </div>
                            }
                        })}
                }
                    .into_any()
            }
        }}
        <p class="hint" style="margin-top:12px">
            "Application deletion removes its history; daemon diagnostics may retain application context."
        </p>
    }
}

/// Stable event kinds read better as words; failures are highlighted.
fn kind_badge(event: &Event) -> AnyView {
    let failure = event.error_code.is_some() || event.kind.contains("fail");
    let tone = if failure {
        Tone::Bad
    } else if event.kind.contains("succeeded") || event.kind.contains("recover") {
        Tone::Ok
    } else {
        Tone::Neutral
    };
    badge(tone, event.kind.replace('_', " "))
}

/// Summary card for one event, linking to related operation events and, when
/// present, its diagnostic details.
#[component]
fn EventCard(event: Event, #[prop(optional)] scoped: bool) -> impl IntoView {
    let diagnostic = event.diagnostic.as_ref().map(|d| d.id.clone());
    let meta = [
        Some(event.scope.as_str().to_owned()),
        event.phase.clone(),
        event.resource.clone(),
        event.attempt.map(|attempt| format!("attempt {attempt}")),
        event.retry.map(|retry| format!("request {retry}")),
        event
            .duration_ms
            .map(|ms| duration(i64::try_from(ms).unwrap_or(i64::MAX))),
        event.error_code.clone(),
        event.action_id.clone().map(|id| format!("action {id}")),
    ];
    view! {
        <article class="event">
            <span class="event-time">{when(event.created_at_ms)}</span>
            <div>
                <div class="event-title">
                    {kind_badge(&event)} <span>{event.message.clone().unwrap_or_default()}</span>
                </div>
                <div class="event-meta">
                    {meta
                        .into_iter()
                        .flatten()
                        .map(|item| view! { <span>{item}</span> })
                        .collect_view()}
                </div>
                <div class="event-links">
                    {
                        let signals = dashboard_context().signals;
                        let environment = event.environment_id.filter(|_| !scoped);
                        move || {
                            environment
                                .as_ref()
                                .and_then(|id| environment_link(signals, id.as_str()))
                                .map(|(label, href)| view! { <A href={href}>{label}</A> })
                        }
                    }
                    {event
                        .actor_credential_id
                        .clone()
                        .map(|id| {
                            view! {
                                <A href={format!("/dashboard/audit?credential={id}")}>
                                    "Requester"
                                </A>
                            }
                        })}
                    {event
                        .operation_id
                        .map(|id| {
                            view! {
                                <A href={format!(
                                    "/dashboard/events?operation={id}",
                                )}>"Operation events"</A>
                            }
                        })}
                    {diagnostic
                        .map(|id| {
                            view! {
                                <A href={format!("/dashboard/errors/{id}")}>"Diagnostic details"</A>
                            }
                        })}
                </div>
            </div>
        </article>
    }
}

/// `/errors/:id` page: loads one diagnostic event by ID.
#[component]
pub(super) fn DiagnosticPage() -> impl IntoView {
    let params = use_params_map();
    let data = LocalResource::new(move || {
        let id = params.with(|p| p.get("id").unwrap_or_default());
        async move {
            Client::browser()
                .diagnostic(&id)
                .await
                .map_err(|e| client_error_message(&e))
        }
    });
    view! {
        <nav class="breadcrumb" aria-label="Breadcrumb">
            <A href="/dashboard/errors">"Errors"</A>
            {icon(Icon::ChevronRight)}
            <span>"Diagnostic"</span>
        </nav>
        <PageHeader title="Diagnostic details" />
        {move || match data.get() {
            None => empty("Loading diagnostic…"),
            Some(Err(e)) => notice(Tone::Bad, e),
            Some(Ok(event)) => view! { <DiagnosticDetails event={event} /> }.into_any(),
        }}
    }
}

/// Full diagnostic view: event context, causes, suggested next action and retryability.
#[component]
fn DiagnosticDetails(event: Event) -> impl IntoView {
    let detail = event.diagnostic.clone();
    let operation = event.operation_id.clone();
    view! {
        <div class="stack">
            {detail
                .map(|d| {
                    view! {
                        <section class="card">
                            <header>
                                <div>
                                    <h2>{d.code}</h2>
                                    <p>{d.summary}</p>
                                </div>
                                {badge(
                                    if d.retryable { Tone::Info } else { Tone::Warn },
                                    if d.retryable { "retryable" } else { "needs attention" },
                                )}
                            </header>
                            <div class="stack-sm">
                                {(!d.causes.is_empty())
                                    .then(|| {
                                        view! {
                                            <div>
                                                <h3>"Causes"</h3>
                                                <ul style="padding-left:1.2em;list-style:disc">
                                                    {d
                                                        .causes
                                                        .into_iter()
                                                        .map(|cause| view! { <li>{cause}</li> })
                                                        .collect_view()}
                                                </ul>
                                            </div>
                                        }
                                    })}
                                {notice(
                                    Tone::Info,
                                    view! {
                                        <strong>"Next action"</strong>
                                        {d.next_action}
                                        {if d.retryable {
                                            " Automatic recovery is supported; inspect related events for the latest outcome."
                                        } else {
                                            " Administrator intervention may be required."
                                        }}
                                    },
                                )}
                            </div>
                        </section>
                    }
                })} <section class="card">
                <header>
                    <h3>"Context"</h3>
                </header>
                <dl class="kv">
                    <dt>"Recorded"</dt>
                    <dd>{timestamp(event.created_at_ms)}</dd>
                    <dt>"Environment"</dt>
                    <dd>
                        {event
                            .environment_id
                            .clone()
                            .map_or_else(
                                || "Daemon".into_any(),
                                |id| {
                                    environment_link(dashboard_context().signals, id.as_str())
                                        .map_or_else(
                                            || view! { <code>{id.to_string()}</code> }.into_any(),
                                            |(label, href)| view! { <A href={href}>{label}</A> }.into_any(),
                                        )
                                },
                            )}
                    </dd>
                    <dt>"Operation"</dt>
                    <dd>
                        {operation
                            .clone()
                            .map_or_else(
                                || "None".into_any(),
                                |id| {
                                    view! {
                                        <A href={format!(
                                            "/dashboard/events?operation={id}",
                                        )}>{id}</A>
                                    }
                                        .into_any()
                                },
                            )}
                    </dd>
                    <dt>"Action"</dt>
                    <dd>{event.action_id.clone().unwrap_or_else(|| "None".into())}</dd>
                    <dt>"Request"</dt>
                    <dd>{event.request_id.clone().unwrap_or_else(|| "None".into())}</dd>
                    <dt>"Diagnostic ID"</dt>
                    <dd>
                        <code>
                            {event.diagnostic.as_ref().map(|d| d.id.clone()).unwrap_or_default()}
                        </code>
                    </dd>
                </dl>
            </section> <section class="list">
                <EventCard event={event} />
            </section>
        </div>
    }
}

/// `/system` page: readiness panel plus auto-refreshing daemon resource usage.
#[component]
pub(super) fn SystemPage() -> impl IntoView {
    let refresh = Refresh::new();
    let data = LocalResource::new(move || {
        refresh.0.track();
        async move {
            Client::browser()
                .daemon_stats()
                .await
                .map_err(|e| client_error_message(&e))
        }
    });
    view! {
        <PageHeader
            title="Daemon status"
            description="Deployment prerequisites and live resource usage of the piqueld process."
        >
            <button type="button" class="btn" on:click={move |_| refresh.request()}>
                {icon(Icon::Refresh)}
                "Refresh"
            </button>
        </PageHeader>
        <div class="stack">
            <super::dashboard::ReadinessPanel />
            {move || match data.get() {
                None => empty("Collecting resource usage…"),
                Some(Err(e)) => notice(Tone::Bad, e),
                Some(Ok(stats)) => resource_stats(&stats),
            }}
        </div>
    }
}

fn resource_stats(stats: &DaemonStats) -> AnyView {
    let unavailable = || "Unavailable".to_owned();
    let counters = [
        ("Retained events", stats.events.to_string()),
        ("Diagnostic occurrences", stats.diagnostics.to_string()),
        (
            "Retained build output",
            bytes(u64::try_from(stats.build_output_bytes).unwrap_or(0)),
        ),
        ("Running operations", stats.running_operations.to_string()),
        ("Queued operations", stats.queued_operations.to_string()),
        ("Pending deliveries", stats.pending_deliveries.to_string()),
        ("Failed deliveries", stats.failed_deliveries.to_string()),
    ];
    view! {
        <div class="metrics">
            {metric("Uptime", duration_secs(stats.uptime_seconds), None::<&str>)}
            {metric("Memory", stats.memory_bytes.map_or_else(unavailable, bytes), Some("resident"))}
            {metric(
                "CPU",
                stats.cpu_percent.map_or_else(|| "Collecting".into(), |v| format!("{v:.1}%")),
                Some("100% is one core"),
            )}
            {metric(
                "Database",
                bytes(stats.database_bytes),
                Some(format!("WAL {}", bytes(stats.wal_bytes))),
            )}
            {metric(
                "Available disk",
                stats.available_disk_bytes.map_or_else(unavailable, bytes),
                Some("data directory"),
            )}
        </div>
        <section class="card">
            <header>
                <div>
                    <h3>"Counters"</h3>
                    <p>
                        {format!(
                            "Measured {}. Historical graphs require an external metrics collector.",
                            timestamp(stats.sampled_at_ms),
                        )}
                    </p>
                </div>
            </header>
            <dl class="kv">
                {counters
                    .into_iter()
                    .map(|(label, value)| {
                        view! {
                            <dt>{label}</dt>
                            <dd>{value}</dd>
                        }
                    })
                    .collect_view()}
            </dl>
        </section>
    }
    .into_any()
}

/// `/analytics` page: deployment outcome, retry, duration and failure-code
/// aggregates over a selectable period.
#[component]
pub(super) fn AnalyticsPage() -> impl IntoView {
    let refresh = Refresh::new();
    let days = RwSignal::new(30_u32);
    let data = LocalResource::new(move || {
        refresh.0.track();
        let days = days.get();
        async move {
            Client::browser()
                .deployment_analytics(None, Refresh::since(days), None)
                .await
                .map_err(|e| client_error_message(&e))
        }
    });
    view! {
        <PageHeader
            title="Analytics"
            description="Deployment outcomes and action timings derived from retained history."
        >
            {period_select(days, false, || {})}
        </PageHeader>
        {move || match data.get() {
            None => empty("Loading analytics…"),
            Some(Err(e)) => notice(Tone::Bad, e),
            Some(Ok(a)) => {
                view! {
                    <div class="stack">
                        {a
                            .incomplete
                            .then(|| {
                                notice(
                                    Tone::Warn,
                                    "This interval includes unavailable detailed history. Counts describe retained applications and records only.",
                                )
                            })}
                        <div class="metrics">
                            {metric(
                                "Deployments",
                                a.deployments.to_string(),
                                Some("with terminal attempts"),
                            )}
                            {metric("Succeeded", a.succeeded.to_string(), Some("latest attempt"))}
                            {metric("Failed", a.failed.to_string(), Some("latest attempt"))}
                            {metric(
                                "Mean attempt",
                                a.mean_duration_ms.map_or_else(|| "—".into(), duration_f64),
                                Some("completed deployment attempts"),
                            )}
                        </div>
                        <div class="metrics">
                            {metric(
                                "Failed attempts",
                                a.failed_attempts.to_string(),
                                Some("including later recoveries"),
                            )}
                            {metric(
                                "Retry attempts",
                                a.retry_attempts.to_string(),
                                Some("attempts beyond the first"),
                            )}
                            {metric(
                                "Action retries",
                                a.action_retries.to_string(),
                                Some("Docker actions retried"),
                            )}
                        </div> <section class="card card-flush">
                            <header>
                                <h3>"Action durations"</h3>
                            </header>
                            {if a.actions.is_empty() {
                                empty("No completed actions in this interval.")
                            } else {
                                view! {
                                    <table class="table">
                                        <thead>
                                            <tr>
                                                <th>"Phase"</th>
                                                <th class="num">"Actions"</th>
                                                <th class="num">"Mean duration"</th>
                                            </tr>
                                        </thead>
                                        <tbody>
                                            {a
                                                .actions
                                                .into_iter()
                                                .map(|r| {
                                                    view! {
                                                        <tr>
                                                            <td>{r.phase}</td>
                                                            <td class="num">{r.count}</td>
                                                            <td class="num">{duration_f64(r.mean_ms)}</td>
                                                        </tr>
                                                    }
                                                })
                                                .collect_view()}
                                        </tbody>
                                    </table>
                                }
                                    .into_any()
                            }}
                        </section> <section class="card card-flush">
                            <header>
                                <h3>"Common failures"</h3>
                            </header>
                            {if a.failures.is_empty() {
                                empty("No failures recorded in this interval.")
                            } else {
                                view! {
                                    <table class="table">
                                        <thead>
                                            <tr>
                                                <th>"Code"</th>
                                                <th class="num">"Occurrences"</th>
                                            </tr>
                                        </thead>
                                        <tbody>
                                            {a
                                                .failures
                                                .into_iter()
                                                .map(|r| {
                                                    view! {
                                                        <tr>
                                                            <td>
                                                                <code>{r.code}</code>
                                                            </td>
                                                            <td class="num">{r.count}</td>
                                                        </tr>
                                                    }
                                                })
                                                .collect_view()}
                                        </tbody>
                                    </table>
                                }
                                    .into_any()
                            }}
                        </section>
                    </div>
                }
                    .into_any()
            }
        }}
    }
}

/// `/notifications` page: paginated notification deliveries, with a retry
/// button on failed ones.
#[component]
pub(super) fn NotificationsPage() -> impl IntoView {
    let refresh = Refresh::new();
    let cursor = RwSignal::new(None::<String>);
    let error = RwSignal::new(None::<String>);
    let data = LocalResource::new(move || {
        refresh.0.track();
        let cursor = cursor.get();
        async move {
            Client::browser()
                .notification_deliveries(cursor.as_deref())
                .await
                .map_err(|e| client_error_message(&e))
        }
    });
    let retry = move |id: String| {
        spawn_local(async move {
            match Client::browser().retry_notification(&id).await {
                Ok(()) => {
                    error.set(None);
                    refresh.request();
                }
                Err(e) => error.set(Some(client_error_message(&e))),
            }
        });
    };
    view! {
        <PageHeader
            title="Notifications"
            description="Webhook deliveries. Destinations and categories are configured in the daemon TOML; delivery may repeat after a lost acknowledgement."
        >
            <button
                type="button"
                class="btn"
                on:click={move |_| {
                    cursor.set(None);
                    refresh.request();
                }}
            >
                {icon(Icon::Refresh)}
                "Newest"
            </button>
        </PageHeader>
        <div class="stack">
            {move || error.get().map(|e| notice(Tone::Bad, e))}
            {move || match data.get() {
                None => empty("Loading deliveries…"),
                Some(Err(e)) => notice(Tone::Bad, e),
                Some(Ok(page)) => {
                    view! {
                        <div class="table-wrap">
                            {if page.items.is_empty() {
                                empty("No notification deliveries.")
                            } else {
                                view! {
                                    <table class="table">
                                        <thead>
                                            <tr>
                                                <th>"Destination"</th>
                                                <th>"Category"</th>
                                                <th>"State"</th>
                                                <th class="num">"Attempts"</th>
                                                <th>"Updated"</th>
                                                <th>"Last error"</th>
                                                <th></th>
                                            </tr>
                                        </thead>
                                        <tbody>
                                            {page
                                                .items
                                                .into_iter()
                                                .map(|d| {
                                                    let id = d.id.clone();
                                                    let tone = match d.state {
                                                        DeliveryState::Delivered => Tone::Ok,
                                                        DeliveryState::Failed => Tone::Bad,
                                                        DeliveryState::Pending => Tone::Pending,
                                                        DeliveryState::Cancelled => Tone::Neutral,
                                                    };
                                                    view! {
                                                        <tr>
                                                            <td title={d.id.clone()}>{d.destination}</td>
                                                            <td>{d.category.to_string()}</td>
                                                            <td>{badge(tone, d.state.to_string())}</td>
                                                            <td class="num">{d.attempts}</td>
                                                            <td class="muted">{when(d.updated_at_ms)}</td>
                                                            <td class="muted">{d.last_error.unwrap_or_default()}</td>
                                                            <td class="actions">
                                                                {(d.state == DeliveryState::Failed)
                                                                    .then(|| {
                                                                        view! {
                                                                            <button
                                                                                type="button"
                                                                                class="btn btn-sm"
                                                                                on:click={move |_| retry(id.clone())}
                                                                            >
                                                                                "Retry"
                                                                            </button>
                                                                        }
                                                                    })}
                                                            </td>
                                                        </tr>
                                                    }
                                                })
                                                .collect_view()}
                                        </tbody>
                                    </table>
                                }
                                    .into_any()
                            }}
                        </div>
                        {page
                            .next_cursor
                            .map(|next| {
                                view! {
                                    <div class="btn-group">
                                        <button
                                            type="button"
                                            class="btn"
                                            on:click={move |_| cursor.set(Some(next.clone()))}
                                        >
                                            "Older deliveries"
                                        </button>
                                    </div>
                                }
                            })}
                    }
                        .into_any()
                }
            }}
        </div>
    }
}

/// Audit trail: refused requests, writes, and sensitive reads, newest first.
/// Accounts without `audit:read` see only their own; `?credential=<id>`
/// narrows the trail to one session or token.
#[component]
pub(super) fn AuditPage() -> impl IntoView {
    use piqueld_client::audit::{AuditFilter, AuditOutcome};
    let query = use_query_map();
    let cursor = RwSignal::new(None::<String>);
    let outcome = RwSignal::new(None::<AuditOutcome>);
    let data = LocalResource::new(move || {
        let filter = AuditFilter {
            user_id: None,
            credential_id: query.with(|query| query.get("credential")),
            outcome: outcome.get(),
        };
        let cursor = cursor.get();
        async move {
            Client::browser()
                .audit_events(&filter, cursor.as_deref(), 50)
                .await
                .map_err(|e| client_error_message(&e))
        }
    });
    view! {
        <PageHeader
            title="Audit"
            description="Refused requests, changes, and reads of logs, configuration, and accounts. Without audit:read, only your own account's requests are shown."
        >
            <label class="field">
                <span>"Outcome"</span>
                <select on:change={move |event| {
                    cursor.set(None);
                    outcome.set(AuditOutcome::parse(&event_target_value(&event)));
                }}>
                    <option value="">"Any"</option>
                    <option value="allowed">"Allowed"</option>
                    <option value="denied">"Denied"</option>
                    <option value="failed">"Failed"</option>
                </select>
            </label>
        </PageHeader>
        <div class="stack">
            {move || match data.get() {
                None => empty("Loading audit trail…"),
                Some(Err(e)) => notice(Tone::Bad, e),
                Some(Ok(page)) => {
                    view! {
                        <div class="table-wrap">
                            {if page.items.is_empty() {
                                empty("No audited requests.")
                            } else {
                                view! {
                                    <table class="table">
                                        <thead>
                                            <tr>
                                                <th>"When"</th>
                                                <th>"Outcome"</th>
                                                <th>"Request"</th>
                                                <th>"Account"</th>
                                                <th>"Credential"</th>
                                                <th>"From"</th>
                                            </tr>
                                        </thead>
                                        <tbody>
                                            {page
                                                .items
                                                .into_iter()
                                                .map(|event| {
                                                    let tone = match event.outcome {
                                                        AuditOutcome::Allowed => Tone::Ok,
                                                        AuditOutcome::Denied => Tone::Bad,
                                                        AuditOutcome::Failed => Tone::Warn,
                                                    };
                                                    let target = event.target();
                                                    let missing = event
                                                        .permission
                                                        .map(|permission| format!(" (requires {permission})"));
                                                    view! {
                                                        <tr>
                                                            <td class="muted">{when(event.created_at_ms)}</td>
                                                            <td>{badge(tone, event.outcome.as_str())}</td>
                                                            <td>
                                                                <code>{event.action}</code>
                                                                {format!(" {}", event.status)}
                                                                {missing}
                                                                {target
                                                                    .map(|target| {
                                                                        view! { <div class="muted">{target}</div> }
                                                                    })}
                                                            </td>
                                                            <td>
                                                                {event
                                                                    .username
                                                                    .or(event.user_id)
                                                                    .unwrap_or_else(|| "anonymous".into())}
                                                            </td>
                                                            <td class="muted" title={event.credential_id.clone()}>
                                                                {event.credential_kind.unwrap_or_default()}
                                                                {event
                                                                    .scoped
                                                                    .unwrap_or(false)
                                                                    .then_some(" (limited)")}
                                                            </td>
                                                            <td class="muted">
                                                                {event.peer.unwrap_or_else(|| "Unix socket".into())}
                                                            </td>
                                                        </tr>
                                                    }
                                                })
                                                .collect_view()}
                                        </tbody>
                                    </table>
                                }
                                    .into_any()
                            }}
                        </div>
                        {page
                            .next_cursor
                            .map(|next| {
                                view! {
                                    <div class="btn-group">
                                        <button
                                            type="button"
                                            class="btn"
                                            on:click={move |_| cursor.set(Some(next.clone()))}
                                        >
                                            "Older requests"
                                        </button>
                                    </div>
                                }
                            })}
                    }
                        .into_any()
                }
            }}
        </div>
    }
}
