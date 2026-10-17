//! Event history, diagnostics, daemon statistics and settings, analytics,
//! notification deliveries, and the audit trail.
use super::format::{bytes, duration, duration_f64, duration_secs, now_ms, timestamp};
use super::ui::{
    Icon, PageHeader, Tone, badge, empty, icon, key_values, loaded, metric, notice, when,
};
use super::{ClientErrorExt, UserError, access::can, dashboard_context};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::{use_params_map, use_query_map};
use piqueld_client::{
    Client, Event, EventActor,
    access::{GlobalPermission, Permission},
    audit::{AuditEvent, AuditFilter, AuditLink, AuditOutcome},
    observability::{
        DaemonStats, DeliveryState, DeploymentAnalytics, EventFilter, EventScope,
        NotificationDelivery,
    },
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

    /// Subscribes the running resource to every refresh.
    fn track(self) {
        self.0.track();
    }

    /// Triggers an immediate refetch.
    fn request(self) {
        self.0.update(|r| *r = r.wrapping_add(1));
    }
}

/// How many days of history a page reads, from its period picker; `0` reads
/// all retained history.
#[derive(Clone, Copy)]
struct Period(RwSignal<u32>);

impl Period {
    /// Starts at the last 30 days.
    fn new() -> Self {
        Self(RwSignal::new(30))
    }

    /// Unix milliseconds the period starts at, or `None` (no lower bound)
    /// for all history. Tracked.
    fn since(self) -> Option<i64> {
        let days = self.0.get();
        (days != 0).then(|| now_ms().saturating_sub(i64::from(days) * 86_400_000))
    }

    /// The picker. Its longest choice is all retained history with `all`,
    /// else 365 days; `changed` runs after a choice so lists can restart at
    /// the newest page.
    fn select(self, all: bool, changed: impl Fn() + 'static) -> AnyView {
        view! {
            <label class="field">
                <span>"Period"</span>
                <select on:change={move |e| {
                    self.0.set(event_target_value(&e).parse().unwrap_or(30));
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
}

/// Button opening the `next` page of a paginated list in `cursor`.
fn older_page(
    cursor: RwSignal<Option<String>>,
    next: Option<String>,
    label: &'static str,
    style: Option<&'static str>,
) -> Option<AnyView> {
    next.map(|next| {
        view! {
            <div class="btn-group" style={style}>
                <button
                    type="button"
                    class="btn"
                    on:click={move |_| cursor.set(Some(next.clone()))}
                >
                    {label}
                </button>
            </div>
        }
        .into_any()
    })
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
/// application (with all its environments) or one environment. Also honours an
/// `?operation=` query filter; changing any filter or the operation resets to
/// the newest page. Auto-refreshes via `Refresh`.
#[component]
#[expect(clippy::too_many_lines, reason = "flat view! markup")]
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
    let period = Period::new();
    let failures = RwSignal::new(errors_only);
    let data = LocalResource::new(move || {
        refresh.track();
        let (application_id, environment_id) = scope_filter.get_value();
        let filter = EventFilter {
            application_id,
            environment_id,
            operation_id: query.with(|query| query.get("operation")),
            kind: Some(kind.get()).filter(|kind| !kind.is_empty()),
            error_code: Some(code.get()).filter(|code| !code.is_empty()),
            scope: match scope.get().as_str() {
                "daemon" => Some(EventScope::Daemon),
                "application" => Some(EventScope::Application),
                _ => None,
            },
            errors_only: failures.get(),
            since_ms: period.since(),
            descending: true,
            ..EventFilter::default()
        };
        let cursor = cursor.get();
        async move {
            Client::browser()
                .filtered_events(&filter, cursor.as_deref(), 50)
                .await
                .user_error()
        }
    });
    let reset = move || cursor.set(None);
    // Handler setting `value` to a filter's new text, restarting at the
    // newest page.
    let filter = move |value: RwSignal<String>| {
        move |event: leptos::ev::Event| {
            value.set(event_target_value(&event));
            reset();
        }
    };
    view! {
        <div class="toolbar">
            {period.select(true, reset)} <Show when={move || !owned}>
                <label class="field">
                    <span>"Scope"</span>
                    <select on:change={filter(scope)}>
                        <option value="">"All"</option>
                        <option value="application">"Application"</option>
                        <option value="daemon">"Daemon"</option>
                    </select>
                </label>
            </Show> <label class="field">
                <span>"Event kind"</span>
                <input placeholder="All kinds" on:change={filter(kind)} />
            </label> <label class="field">
                <span>"Error code"</span>
                <input placeholder="All codes" on:change={filter(code)} />
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
        {loaded(
            data,
            "Loading history…",
            move |page| {
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
                    {older_page(cursor, page.next_cursor, "Older events", Some("margin-top:12px"))}
                }
                    .into_any()
            },
        )}
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

/// Address of the events of operation `id`.
fn operation_events(id: &str) -> String {
    format!("/dashboard/events?operation={id}")
}

/// Summary card for one event, linking to related operation events and, when
/// present, its diagnostic details.
#[component]
fn EventCard(event: Event, #[prop(optional)] scoped: bool) -> impl IntoView {
    let signals = dashboard_context().signals;
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
        match &event.actor {
            Some(actor @ EventActor::System { .. }) => Some(format!("by {actor}")),
            _ => None,
        },
    ];
    let environment = event.environment_id.clone().filter(|_| !scoped);
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
                    {move || {
                        environment
                            .as_ref()
                            .and_then(|id| signals.environment_link(id.as_str()))
                            .map(|(label, href)| view! { <A href={href}>{label}</A> })
                    }}
                    {event
                        .actor
                        .as_ref()
                        .and_then(EventActor::credential_id)
                        .map(|id| {
                            view! {
                                <A href={format!(
                                    "/dashboard/audit?credential={id}",
                                )}>"Requester"</A>
                            }
                        })}
                    {event
                        .operation_id
                        .map(|id| view! { <A href={operation_events(&id)}>"Operation events"</A> })}
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
        async move { Client::browser().diagnostic(&id).await.user_error() }
    });
    view! {
        <nav class="breadcrumb" aria-label="Breadcrumb">
            <A href="/dashboard/errors">"Errors"</A>
            {icon(Icon::ChevronRight)}
            <span>"Diagnostic"</span>
        </nav>
        <PageHeader title="Diagnostic details" />
        {loaded(
            data,
            "Loading diagnostic…",
            |event| view! { <DiagnosticDetails event={event} /> }.into_any(),
        )}
    }
}

/// Full diagnostic view: event context, causes, suggested next action and retryability.
#[component]
fn DiagnosticDetails(event: Event) -> impl IntoView {
    let detail = event.diagnostic.clone();
    let operation = event.operation_id.clone();
    let environment = event.environment_id.clone().map_or_else(
        || "Daemon".into_any(),
        |id| dashboard_context().signals.environment_label(id.as_str()),
    );
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
                    <dd>{environment}</dd>
                    <dt>"Operation"</dt>
                    <dd>
                        {operation
                            .map_or_else(
                                || "None".into_any(),
                                |id| view! { <A href={operation_events(&id)}>{id}</A> }.into_any(),
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
        refresh.track();
        async move { Client::browser().daemon_stats().await.user_error() }
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
            {loaded(data, "Collecting resource usage…", |stats| resource_stats(&stats))}
        </div>
    }
}

/// Metric tiles and counters of the daemon process.
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
            {key_values(counters)}
        </section>
    }
    .into_any()
}

/// `/settings` page: read-only daemon configuration grouped by section.
#[component]
pub(super) fn HostPage() -> impl IntoView {
    let settings = RwSignal::new(None);
    let error = RwSignal::new(None::<String>);
    spawn_local(async move {
        match Client::browser().system_configuration().await {
            Ok(config) => settings.set(Some(config)),
            Err(e) => error.set(Some(e.user_message())),
        }
    });
    view! {
        <PageHeader
            title="Host settings"
            description="Effective daemon configuration. Edit the TOML file and restart the daemon to change these values."
        />
        <div class="stack">
            {move || error.get().map(|e| notice(Tone::Bad, e))}
            {move || {
                settings
                    .get()
                    .map(|config| {
                        config
                            .groups
                            .into_iter()
                            .map(|(group, values)| {
                                view! {
                                    <section class="card">
                                        <header>
                                            <h3>{group}</h3>
                                        </header>
                                        {key_values(values)}
                                    </section>
                                }
                            })
                            .collect_view()
                    })
            }}
        </div>
    }
}

/// `/analytics` page: deployment outcome, retry, duration and failure-code
/// aggregates over a selectable period.
#[component]
pub(super) fn AnalyticsPage() -> impl IntoView {
    let refresh = Refresh::new();
    let period = Period::new();
    let data = LocalResource::new(move || {
        refresh.track();
        let since = period.since();
        async move {
            Client::browser()
                .deployment_analytics(None, since, None)
                .await
                .user_error()
        }
    });
    view! {
        <PageHeader
            title="Analytics"
            description="Deployment outcomes and action timings derived from retained history."
        >
            {period.select(false, || {})}
        </PageHeader>
#[expect(clippy::too_many_lines, reason = "flat view! markup")]
        {loaded(data, "Loading analytics…", analytics)}
    }
}

/// Metric tiles and tables of deployment analytics.
fn analytics(a: DeploymentAnalytics) -> AnyView {
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
                {metric("Deployments", a.deployments.to_string(), Some("with terminal attempts"))}
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

/// `/notifications` page: paginated notification deliveries, with a retry
/// button on failed ones.
#[component]
pub(super) fn NotificationsPage() -> impl IntoView {
    let refresh = Refresh::new();
    let cursor = RwSignal::new(None::<String>);
    let error = RwSignal::new(None::<String>);
    let data = LocalResource::new(move || {
        refresh.track();
        let cursor = cursor.get();
        async move {
            Client::browser()
                .notification_deliveries(cursor.as_deref())
                .await
                .user_error()
        }
    });
    let retry = Callback::new(move |id: String| {
        spawn_local(async move {
            match Client::browser().retry_notification(&id).await {
                Ok(()) => {
                    error.set(None);
                    refresh.request();
                }
                Err(e) => error.set(Some(e.user_message())),
            }
        });
    });
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
            {loaded(
                data,
                "Loading deliveries…",
                move |page| {
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
                                                .map(|delivery| delivery_row(delivery, retry))
                                                .collect_view()}
                                        </tbody>
                                    </table>
                                }
                                    .into_any()
                            }}
                        </div>
                        {older_page(cursor, page.next_cursor, "Older deliveries", None)}
                    }
                        .into_any()
                },
            )}
        </div>
    }
}

/// Table row for one delivery, with a button that runs `retry` on a failed one.
fn delivery_row(delivery: NotificationDelivery, retry: Callback<String>) -> AnyView {
    let id = delivery.id.clone();
    let tone = match delivery.state {
        DeliveryState::Delivered => Tone::Ok,
        DeliveryState::Failed => Tone::Bad,
        DeliveryState::Pending => Tone::Pending,
        DeliveryState::Cancelled => Tone::Neutral,
    };
    view! {
        <tr>
            <td title={delivery.id.clone()}>{delivery.destination}</td>
            <td>{delivery.category.to_string()}</td>
            <td>{badge(tone, delivery.state.to_string())}</td>
            <td class="num">{delivery.attempts}</td>
            <td class="muted">{when(delivery.updated_at_ms)}</td>
            <td class="muted">{delivery.last_error.unwrap_or_default()}</td>
            <td class="actions">
                {(delivery.state == DeliveryState::Failed)
                    .then(|| {
                        view! {
                            <button
                                type="button"
                                class="btn btn-sm"
                                on:click={move |_| retry.run(id.clone())}
                            >
                                "Retry"
                            </button>
                        }
                    })}
            </td>
        </tr>
    }
    .into_any()
}

/// Audit trail: refused requests, writes, and sensitive reads, newest first.
/// Accounts without `audit:read` see only their own; `?credential=<id>`
/// narrows the trail to one session or token.
#[component]
pub(super) fn AuditPage() -> impl IntoView {
    let query = use_query_map();
    let cursor = RwSignal::new(None::<String>);
    let outcome = RwSignal::new(None::<AuditOutcome>);
    let verified = RwSignal::new(None::<(Tone, String)>);
    let data = LocalResource::new(move || {
        let filter = AuditFilter {
            credential_id: query.with(|query| query.get("credential")),
            outcome: outcome.get(),
            ..AuditFilter::default()
        };
        let cursor = cursor.get();
        async move {
            Client::browser()
                .audit_events(&filter, cursor.as_deref(), 50)
                .await
                .user_error()
        }
    });
    view! {
        <PageHeader
            title="Audit"
            description="Refused requests, changes, and reads of logs, configuration, and accounts. Without audit:read, only your own account's requests are shown."
        >
            <AuditVerify result={verified} />
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
            {move || verified.get().map(|(tone, message)| notice(tone, message))}
            {loaded(
                data,
                "Loading audit trail…",
                move |page| {
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
                                            {page.items.into_iter().map(audit_row).collect_view()}
                                        </tbody>
                                    </table>
                                }
                                    .into_any()
                            }}
                        </div>
                        {older_page(cursor, page.next_cursor, "Older requests", None)}
                    }
                        .into_any()
                },
            )}
        </div>
    }
}

/// Table row for one audited request.
fn audit_row(event: AuditEvent) -> AnyView {
    let tone = match event.outcome {
        AuditOutcome::Allowed => Tone::Ok,
        AuditOutcome::Denied => Tone::Bad,
        AuditOutcome::Failed => Tone::Warn,
    };
    let target = event.target();
    let who = event.who();
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
                {target.map(|target| view! { <div class="muted">{target}</div> })}
            </td>
            <td>{who}</td>
            <td class="muted" title={event.credential_id.clone()}>
                {event.credential_kind.unwrap_or_default()}
                {event.scoped.unwrap_or(false).then_some(" (limited)")}
            </td>
            <td class="muted">
                {event.peer.unwrap_or_else(|| "Unix socket".into())}
                {event.tailnet.map(|who| format!(" ({who})"))}
            </td>
        </tr>
    }
    .into_any()
}

/// "Verify integrity" for `audit:read` holders: checks the audit trail's hash
/// chain and sets `result` to whether any record was altered, with the anchor
/// and newest record to keep elsewhere.
#[component]
fn AuditVerify(result: RwSignal<Option<(Tone, String)>>) -> impl IntoView {
    let allowed = can(Permission::Global(GlobalPermission::AuditRead));
    let busy = RwSignal::new(false);
    let verify = move |_| {
        busy.set(true);
        spawn_local(async move {
            result.set(Some(match Client::browser().verify_audit().await {
                Ok(check) => {
                    let link = |link: &Option<AuditLink>| {
                        link.as_ref().map_or_else(
                            || "none".to_owned(),
                            |link| format!("record {} ({})", link.id, link.link),
                        )
                    };
                    match check.broken_at {
                        Some(id) => (
                            Tone::Bad,
                            format!(
                                "Altered: record {id} does not match; it, or a record just before it, was edited, inserted, or removed. Last verified: {}.",
                                link(&check.head)
                            ),
                        ),
                        None => (
                            Tone::Ok,
                            format!(
                                "Intact: {} records. Pruned through: {}. Newest: {}.",
                                check.checked,
                                link(&check.anchor),
                                link(&check.head),
                            ),
                        ),
                    }
                }
                Err(problem) => (Tone::Bad, problem.user_message()),
            }));
            busy.set(false);
        });
    };
    view! {
        <Show when={move || allowed.get()}>
            <button type="button" class="btn" disabled={move || busy.get()} on:click={verify}>
                {icon(Icon::Refresh)}
                "Verify integrity"
            </button>
        </Show>
    }
}
