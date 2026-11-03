//! Event history, diagnostics, daemon statistics and settings, analytics,
//! and the audit trail.
use super::format::{now_ms, timestamp};
use super::ui::{
    Icon, PageHeader, Tone, badge, code_badge, empty, icon, key_values, loaded, loading, metric,
    notice, when,
};
use super::{ClientErrorExt, UserError, access::can, dashboard_context, operation_events};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::{use_params_map, use_query_map};
use piqueld_client::{
    Client, DEFAULT_PAGE_SIZE, Event, EventActor,
    access::{GlobalPermission, Permission},
    audit::{AuditEvent, AuditFilter, AuditLink, AuditOutcome},
    observability::{
        Bytes, DaemonStats, DeploymentAnalytics, Elapsed, EventFilter, EventScope, Uptime,
    },
    system::{CheckStatus, SystemCheck},
};
use std::collections::BTreeSet;
use std::time::Duration;

/// Revision counter that observability resources track to refetch.
/// Bumped every 5 seconds while the tab is visible, or on demand.
#[derive(Clone, Copy)]
pub(super) struct Refresh(RwSignal<u64>);

impl Refresh {
    /// Starts the 5-second interval, cleared when the owning view unmounts.
    pub(super) fn new() -> Self {
        let revision = RwSignal::new(0_u64);
        if let Ok(handle) = set_interval_with_handle(
            move || {
                if !super::document_hidden() {
                    revision.update(|r| *r = r.wrapping_add(1));
                }
            },
            Duration::from_secs(5),
        ) {
            on_cleanup(move || handle.clear());
        }
        Self(revision)
    }

    /// Subscribes the running resource to every refresh.
    pub(super) fn track(self) {
        self.0.track();
    }

    /// Triggers an immediate refetch.
    pub(super) fn request(self) {
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
pub(super) fn older_page(
    cursor: RwSignal<Option<String>>,
    next: Option<String>,
    label: &'static str,
) -> Option<AnyView> {
    next.map(|next| {
        view! {
            <div class="btn-group mt-sm">
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
            description="Everything piqueld did, newest first: deployments and their steps, configuration changes, and failures. Deleting an application removes its history."
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
            description="Every failure, newest first. Each one's diagnostic explains what went wrong and what to do."
        />
        <EventHistory errors_only=true />
    }
}

/// Filterable, paginated event list (`DEFAULT_PAGE_SIZE` per page), optionally
/// scoped to one application (with all its environments), one environment,
/// or one `operation`. Also honours an `?operation=` query filter; changing
/// any filter or the operation resets to the newest page. Auto-refreshes via
/// `Refresh`.
#[component]
#[expect(clippy::too_many_lines, reason = "flat view! markup")]
pub(super) fn EventHistory(
    #[prop(optional, into)] application: Option<String>,
    #[prop(optional, into)] environment: Option<String>,
    #[prop(optional, into)] operation: Option<String>,
    #[prop(optional)] errors_only: bool,
) -> impl IntoView {
    let refresh = Refresh::new();
    let scoped = environment.is_some();
    let owned = scoped || application.is_some();
    let scope_filter = StoredValue::new((application, environment, operation));
    let query = use_query_map();
    // The operation shown: the one this list is about, else the address's.
    let operation = move || {
        scope_filter
            .with_value(|(_, _, operation)| operation.clone())
            .or_else(|| query.with(|query| query.get("operation")))
    };
    let cursor = RwSignal::new(None::<String>);
    // The events whose details are shown, kept across refreshes.
    let expanded = RwSignal::new(BTreeSet::new());
    Effect::new(move |previous: Option<Option<String>>| {
        let operation = operation();
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
    // The kinds on offer, read once and again on Newest.
    let kinds_revision = RwSignal::new(0_u32);
    let kinds = LocalResource::new(move || {
        kinds_revision.track();
        async move { Client::browser().event_kinds().await.user_error() }
    });
    let data = LocalResource::new(move || {
        refresh.track();
        let (application_id, environment_id, _) = scope_filter.get_value();
        let filter = EventFilter {
            application_id,
            environment_id,
            operation_id: operation(),
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
                .filtered_events(&filter, cursor.as_deref(), DEFAULT_PAGE_SIZE)
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
    // The address's operation filter is the page's, so it is cleared by link.
    let filtered_by_address = move || {
        scope_filter.with_value(|(_, _, operation)| operation.is_none())
            && query.with(|query| query.get("operation").is_some())
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
                <select prop:value={move || kind.get()} on:change={filter(kind)}>
                    <option value="">"All kinds"</option>
                    {move || {
                        kinds
                            .get()
                            .and_then(Result::ok)
                            .unwrap_or_default()
                            .into_iter()
                            .map(|name| {
                                view! { <option value={name.clone()}>{words(&name)}</option> }
                            })
                            .collect_view()
                    }}
                </select>
            </label> <label class="field">
                <span>"Error code"</span>
                <input placeholder="All codes" on:change={filter(code)} />
            </label> <Show when={move || !errors_only}>
                <label class="checkbox toolbar-check">
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
            </Show> <div class="toolbar-end">
                <button
                    type="button"
                    class="btn"
                    on:click={move |_| {
                        reset();
                        kinds_revision.update(|revision| *revision = revision.wrapping_add(1));
                        refresh.request();
                    }}
                >
                    {icon(Icon::Refresh)}
                    "Newest"
                </button>
            </div>
        </div>
        {move || {
            filtered_by_address()
                .then(|| {
                    let id = operation().unwrap_or_default();
                    notice(
                        Tone::Info,
                        view! {
                            <span>"Showing the events of operation " <code>{id}</code></span>
                            <div class="btn-group">
                                <A attr:class="btn btn-sm" href="/dashboard/events">
                                    "Show all events"
                                </A>
                            </div>
                        },
                    )
                })
        }}
        {loaded(
            data,
            "Loading history…",
            move |page| {
                view! {
                    <div class="list">
                        {if page.items.is_empty() {
                            empty("No events match these filters.")
                        } else {
                            page.items
                                .into_iter()
                                .map(|event| {
                                    view! {
                                        <EventCard event={event} scoped={scoped} expanded={expanded} />
                                    }
                                })
                                .collect_view()
                                .into_any()
                        }}
                    </div>
                    {older_page(cursor, page.next_cursor, "Older events")}
                }
                    .into_any()
            },
        )}
    }
}

/// An event kind or other identifier as words: `action_succeeded` reads
/// "action succeeded".
fn words(identifier: &str) -> String {
    identifier.replace('_', " ")
}

/// What an event is about, read first: its message, unless that only
/// repeats its kind; else the resource it concerns, else its phase (action
/// events name their action there, such as `ensure_swarm`); else its kind as
/// words.
fn headline(event: &Event) -> String {
    let kind = words(&event.kind);
    event
        .message
        .clone()
        .filter(|message| !message.is_empty() && *message != kind)
        .or_else(|| event.resource.clone())
        .or_else(|| event.phase.clone())
        .unwrap_or(kind)
}

/// The event's kind as a badge, toned by its outcome: failures, successes
/// and recoveries stand out; the rest is neutral.
fn kind_badge(event: &Event) -> AnyView {
    let kind = event.kind.as_str();
    let tone = if event.error_code.is_some()
        || kind.contains("fail")
        || kind.contains("rejected")
        || kind.contains("expired")
    {
        Tone::Bad
    } else if kind.contains("succeeded") || kind.contains("recover") || kind.contains("completed") {
        Tone::Ok
    } else if kind.contains("cancelled") || kind.contains("superseded") {
        Tone::Warn
    } else {
        Tone::Neutral
    };
    badge(tone, words(kind))
}

/// One event: when it happened, what it is about, its kind, and the context
/// that tells it apart, with every recorded field behind **Details**. Links
/// lead to its environment, its operation's events, the requester's audit
/// trail and its diagnostic. `expanded` holds the IDs of the events showing
/// their details: it belongs to the list, which redraws its cards on every
/// refresh, so an opened card stays open.
#[component]
fn EventCard(
    event: Event,
    #[prop(optional)] scoped: bool,
    expanded: RwSignal<BTreeSet<i64>>,
) -> impl IntoView {
    let signals = dashboard_context().signals;
    let id = event.id;
    let opened = move || expanded.with(|expanded| expanded.contains(&id));
    let title = headline(&event);
    let diagnostic = event.diagnostic.as_ref().map(|d| d.id.clone());
    // Context worth a glance, without repeating the headline.
    let unless_title = |value: &Option<String>| value.clone().filter(|value| *value != title);
    let meta = [
        unless_title(&event.phase),
        unless_title(&event.resource),
        event.attempt.map(|attempt| format!("attempt {attempt}")),
        event
            .duration_ms
            .map(|ms| Elapsed(Duration::from_millis(ms)).to_string()),
    ];
    let environment = event.environment_id.clone().filter(|_| !scoped);
    let actor = event.actor.as_ref().map(ToString::to_string);
    let details = [
        ("Kind", Some(event.kind.clone())),
        ("Scope", Some(event.scope.as_str().to_owned())),
        ("Message", event.message.clone()),
        ("Resource", event.resource.clone()),
        ("Phase", event.phase.clone()),
        ("Attempt", event.attempt.map(|attempt| attempt.to_string())),
        ("Request", event.retry.map(|retry| retry.to_string())),
        ("Duration", event.duration_ms.map(|ms| format!("{ms} ms"))),
        ("Error code", event.error_code.clone()),
        ("Operation", event.operation_id.clone()),
        ("Action", event.action_id.clone()),
        ("Request ID", event.request_id.clone()),
        ("By", actor),
        ("Event", Some(event.id.to_string())),
    ];
    view! {
        <article class="event" data-tone={event.error_code.as_ref().map(|_| "bad")}>
            <span class="event-time">{when(event.created_at_ms)}</span>
            <div class="event-main">
                <div class="event-title">
                    <strong>{title}</strong>
                    {kind_badge(&event)}
                    {event.error_code.clone().map(|code| code_badge(Tone::Bad, code))}
                </div>
                <div class="event-meta">
                    {move || {
                        environment
                            .as_ref()
                            .and_then(|id| signals.environment_link(id.as_str()))
                            .map(|(label, href)| view! { <A href={href}>{label}</A> })
                    }}
                    {meta
                        .iter()
                        .flatten()
                        .map(|item| view! { <span>{item.clone()}</span> })
                        .collect_view()}
                </div>
                <div class="event-links">
                    {event
                        .operation_id
                        .clone()
                        .map(|id| view! { <A href={operation_events(&id)}>"Operation"</A> })}
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
                    {diagnostic
                        .map(|id| {
                            view! { <A href={format!("/dashboard/errors/{id}")}>"Diagnostic"</A> }
                        })}
                </div>
                <dl class="kv event-details" hidden={move || !opened()}>
                    {details
                        .into_iter()
                        .filter_map(|(label, value)| {
                            value
                                .map(|value| {
                                    view! {
                                        <dt>{label}</dt>
                                        <dd>{value}</dd>
                                    }
                                })
                        })
                        .collect_view()}
                </dl>
            </div>
            <button
                type="button"
                class="btn btn-ghost btn-sm event-toggle"
                aria-expanded={move || opened().to_string()}
                on:click={move |_| {
                    expanded
                        .update(|expanded| {
                            if !expanded.remove(&id) {
                                expanded.insert(id);
                            }
                        });
                }}
            >
                {move || if opened() { "Hide" } else { "Details" }}
            </button>
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
        {loaded(
            data,
            "Loading diagnostic…",
            |event| view! { <DiagnosticDetails event={event} /> }.into_any(),
        )}
    }
}

/// Full diagnostic view: what failed and what to do, where it happened, and
/// the events of its operation, or the event itself without one.
#[component]
fn DiagnosticDetails(event: Event) -> impl IntoView {
    let detail = event.diagnostic.clone();
    let operation = event.operation_id.clone();
    // Named once the application listing has loaded.
    let signals = dashboard_context().signals;
    let environment = event.environment_id.clone().map_or_else(
        || "Daemon".into_any(),
        |id| view! { {move || signals.environment_label(id.as_str())} }.into_any(),
    );
    let (code, summary, retryable) = detail.as_ref().map_or_else(
        || (event.kind.clone(), headline(&event), None),
        |d| (d.code.clone(), d.summary.clone(), Some(d.retryable)),
    );
    let context = [
        ("Recorded", Some(timestamp(event.created_at_ms).into_any())),
        ("Environment", Some(environment)),
        (
            "Operation",
            operation.clone().map(|id| {
                view! {
                    <A href={operation_events(&id)}>
                        <code>{id.clone()}</code>
                    </A>
                }
                .into_any()
            }),
        ),
        (
            "Action",
            event
                .action_id
                .clone()
                .map(|id| view! { <code>{id}</code> }.into_any()),
        ),
        (
            "Request ID",
            event
                .request_id
                .clone()
                .map(|id| view! { <code>{id}</code> }.into_any()),
        ),
        (
            "Diagnostic ID",
            detail
                .as_ref()
                .map(|d| view! { <code>{d.id.clone()}</code> }.into_any()),
        ),
    ];
    view! {
        <header class="page-header">
            <div>
                <h1>
                    <code>{code}</code>
                </h1>
                <p>{summary}</p>
            </div>
            <div class="page-actions">
                {retryable
                    .map(|retryable| {
                        badge(
                            if retryable { Tone::Info } else { Tone::Warn },
                            if retryable { "retryable" } else { "needs attention" },
                        )
                    })}
            </div>
        </header>
        <div class="stack">
            {detail
                .map(|d| {
                    view! {
                        {notice(
                            Tone::Info,
                            view! {
                                <strong>"What to do"</strong>
                                {d.next_action}
                                {if d.retryable {
                                    " piqueld retries this on its own; the operation's events below show the latest outcome."
                                } else {
                                    " It will not recover on its own."
                                }}
                            },
                        )}
                        {(!d.causes.is_empty())
                            .then(|| {
                                view! {
                                    <section class="card">
                                        <header>
                                            <h3>"Causes"</h3>
                                        </header>
                                        <ul class="bullets">
                                            {d
                                                .causes
                                                .into_iter()
                                                .map(|cause| view! { <li>{cause}</li> })
                                                .collect_view()}
                                        </ul>
                                    </section>
                                }
                            })}
                    }
                })} <section class="card">
                <header>
                    <h3>"Where"</h3>
                </header>
                {key_values(
                    context
                        .into_iter()
                        .filter_map(|(label, value)| value.map(|value| (label, value))),
                )}
            </section> <section>
                <div class="section-header">
                    <div>
                        <h2>
                            {if operation.is_some() {
                                "Events of this operation"
                            } else {
                                "The event"
                            }}
                        </h2>
                    </div>
                </div>
                {match operation {
                    Some(operation) => view! { <EventHistory operation={operation} /> }.into_any(),
                    None => {
                        view! {
                            <section class="list">
                                <EventCard event={event} expanded={RwSignal::new(BTreeSet::new())} />
                            </section>
                        }
                            .into_any()
                    }
                }}
            </section>
        </div>
    }
}

/// `/system` page: readiness panel, installation checks, and auto-refreshing
/// daemon resource usage.
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
            <ChecksPanel />
            {loaded(data, "Collecting resource usage…", |stats| resource_stats(&stats))}
        </div>
    }
}

/// Installation checks from `GET /api/v1/system/checks`, the same as
/// `piquelctl doctor`. They probe the network and Docker, so they run when
/// the page opens and on demand rather than on the page's refresh interval.
#[component]
fn ChecksPanel() -> impl IntoView {
    let run = RwSignal::new(0_u64);
    let checks = LocalResource::new(move || {
        run.track();
        async move { Client::browser().system_checks().await.user_error() }
    });
    view! {
        <section class="card" aria-labelledby="installation-checks-heading">
            <header>
                <div>
                    <h3 id="installation-checks-heading">"Installation checks"</h3>
                    <p>
                        "What the daemon can verify about its own setup. A passing public URL means this host reaches it; browsers elsewhere also need DNS and network access."
                    </p>
                </div>
                <button
                    type="button"
                    class="btn btn-sm"
                    on:click={move |_| run.update(|r| *r = r.wrapping_add(1))}
                >
                    {icon(Icon::Refresh)}
                    "Run checks"
                </button>
            </header>
            {loaded(
                checks,
                "Running checks…",
                |checks| {
                    view! {
                        <div class="status-grid">
                            {checks.checks.into_iter().map(check_card).collect_view()}
                        </div>
                    }
                        .into_any()
                },
            )}
        </section>
    }
}

/// One installation check, with its next step when action is needed.
fn check_card(check: SystemCheck) -> AnyView {
    let (tone, label) = match check.status {
        CheckStatus::Pass => (Tone::Ok, "Pass"),
        CheckStatus::Warn => (Tone::Warn, "Warning"),
        CheckStatus::Fail => (Tone::Bad, "Failed"),
        CheckStatus::Skip => (Tone::Neutral, "Skipped"),
    };
    view! {
        <article class="status-card" data-tone={tone.attr()}>
            <header>
                <strong>{check.id.label()}</strong>
                {badge(tone, label)}
            </header>
            <p>{check.message}</p>
            {check.next_step.map(|step| view! { <p>"Next step: " {step}</p> })}
        </article>
    }
    .into_any()
}

/// Metric tiles and counters of the daemon process.
fn resource_stats(stats: &DaemonStats) -> AnyView {
    let unavailable = || "Unavailable".to_owned();
    let bytes = |value| Bytes(value).to_string();
    let counters = [
        ("Retained events", stats.events.to_string()),
        ("Diagnostic occurrences", stats.diagnostics.to_string()),
        (
            "Retained build output",
            Bytes::from_i64(stats.build_output_bytes).to_string(),
        ),
        ("Running operations", stats.running_operations.to_string()),
        ("Queued operations", stats.queued_operations.to_string()),
        ("Pending deliveries", stats.pending_deliveries.to_string()),
        ("Failed deliveries", stats.failed_deliveries.to_string()),
    ];
    view! {
        <div class="metrics">
            {metric(
                "Uptime",
                Uptime(Duration::from_secs(stats.uptime_seconds)).to_string(),
                None::<&str>,
            )}
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
                            "Measured {}. For history, export telemetry to a collector over OTLP.",
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
            {move || match (settings.get(), error.get()) {
                (Some(config), _) => {
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
                        .into_any()
                }
                (None, Some(e)) => notice(Tone::Bad, e),
                (None, None) => loading("Loading the configuration…"),
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
        {loaded(data, "Loading analytics…", analytics)}
    }
}

/// Metric tiles and tables of deployment analytics.
#[expect(clippy::too_many_lines, reason = "flat view! markup")]
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
                    a
                        .mean_duration_ms
                        .map_or_else(
                            || "—".into(),
                            |ms| Elapsed::from_millis_f64(ms).to_string(),
                        ),
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
                                                <td class="num">
                                                    {Elapsed::from_millis_f64(r.mean_ms).to_string()}
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
                .audit_events(&filter, cursor.as_deref(), DEFAULT_PAGE_SIZE)
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
                        {older_page(cursor, page.next_cursor, "Older requests")}
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
