# Observability

piqueld keeps structured control-plane history in SQLite. Operation records are
execution state; events are the independently readable historical record. Metrics
exposure is optional. There is no OTLP destination, external log exporter,
collector management, or external daemon-outage monitoring in this implementation.

## History and diagnostics

Saved typed edits record their field and resource in application history without
copying configuration values.

An operation groups execution attempts; an attempt groups actions. Action events
carry an action ID, operation ID, application ID, generation, attempt, phase,
resource, request/retry number, and completed duration where applicable. Source
preparation and runtime mutations commit intent before executing. Each mutating
request commits an `action_requested` event before calling Docker. Failure to
write the journal prevents that request. Retries retain diagnostic occurrences
and their scheduled delay. Successful unchanged observation polls do not produce
history.

An interrupted action becomes `action_outcome_unknown`: this does not claim that
Docker rolled back or that nothing happened. Reconciliation inspects actual state
before continuing. A failure after a successful external request but before its
result commits has the same uncertainty. Journal availability is a prerequisite
for infrastructure changes, including startup Swarm initialization and repair.
Execution cleanup waits for in-flight repair to commit before closing abandoned
actions that share the operation's context.

Diagnostics contain an occurrence ID, stable code, safe summary, sanitized causal
facts, retryability and suggested next action. Context stays on the event even
when its operation is pruned. API failures include `details.diagnostic_id`, and
the diagnostic event records the request ID. Input validation errors are ordinary
API errors, not persisted incidents. Daemon logs include occurrence IDs. If
storage itself fails, the response/log ID remains useful but the diagnostic may
not be retrievable from the database.

API failure occurrences are not sampled or deduplicated: repeated failed requests,
including dashboard polling during an outage, each retain their request ID and
diagnostic. Use nonzero retention settings to bound historical growth; webhook
incident deduplication does not reduce the diagnostic journal's write volume.

Safe causal facts include Docker request stages and HTTP statuses, I/O error
kinds and OS codes, command stages and exit codes, database failure kinds/codes,
and validated compilation diagnostics. Preparation and execution use the same
extraction rules; arbitrary source messages and command output stay excluded.

Raw engine response bodies, manifest environment values, webhook URLs and receiver
response bodies are excluded from diagnostics. Build output remains a separate,
bounded log and may contain text emitted by the build itself.

## Ownership and retention

`scope` determines ownership, independently of a contextual `application_id`:

- `application`: removed with application deletion, including diagnostic history,
  deployment/build history, and associated webhook deliveries.
- `daemon`: shared infrastructure or internal failures remain under daemon
  retention, even when they refer to an application that has since been deleted.

`retention.event_days = 0` and `retention.daemon_event_days = 0` both default to no
age-based pruning. Nonzero values independently enable pruning for the two
scopes. Executing actions, open notification incidents, and pending deliveries
protect their source events. An open incident retains its failure delivery history
until recovery, even when the failure was already acknowledged.
Build output retains its existing byte and age limits. Operation pruning does
not make event details unreadable. SQLite file size need not immediately shrink
when rows are deleted; free pages can be reused.

Analytics reports when an interval extends before detailed history began or
includes explicitly pruned history. Aggregates cover retained applications;
deleting an application removes its contribution. Event streaming detects
retention gaps conservatively across both scopes. Deleting an application's
history intentionally removes that data rather than exposing deletion tombstones.

## API and dashboard

All administrative routes use the existing API access boundary. See the generated
[OpenAPI document](openapi-v1.json) for exact contracts.

| Route | Purpose |
| --- | --- |
| `GET /api/v1/events` | Cursor pagination and filtering |
| `GET /api/v1/events/stream` | Replay and follow committed history through SSE |
| `GET /api/v1/diagnostics/{id}` | Original event and details for an occurrence |
| `GET /api/v1/system/resources` | Cached process, storage and queue measurements |
| `GET /api/v1/analytics/deployments` | Deployment outcomes, retries, durations and failure codes |
| `GET /api/v1/notifications/deliveries` | Credential-free delivery history |
| `POST /api/v1/notifications/deliveries/{id}/retry` | Retry a failed delivery under current policy |

Event filters: `application_id`, `operation_id`, `attempt`, `action_id`, `kind`,
`error_code`, `errors_only`, `scope`, `since_ms`, `until_ms`, and `descending`.
Pages contain at most 100 records and use opaque `v1:<id>` cursors. Oldest-first
ordering remains the default. Timestamps are Unix milliseconds.

SSE uses the same filters with oldest-first ordering. Set `Last-Event-ID` to the
last received SSE ID to reconnect (it takes precedence over `cursor`). Without a
cursor, replay starts at retained history. Events are read from the journal in
bounded pages; slow clients do not block writers. A stale resume position receives
HTTP 410, or a terminal `history_expired` event if pruning happens during the
stream. A terminal `stream_error` indicates a storage failure. Clients should
refresh their snapshot before reconnecting after an expired history response.

The dashboard adds History, Errors, Analytics, Notifications and Daemon status.
Application pages include scoped history; diagnostics link to related operation
history. Error feedback links to the specific diagnostic where available. Views
refresh while visible. Configuration is displayed read-only, with destination
URLs omitted.

Resource snapshots are cached for five seconds. They include process uptime,
resident memory and CPU, DB/WAL sizes, available disk, retained event/diagnostic
and build-output counts, operation queues, and pending/failed deliveries. OS
measurements that are unavailable are null and omitted from metrics, not zero.
These are current samples; piqueld does not persist a resource time series.

## Webhooks

Configuration is loaded at startup; restart after changing it. Example:

```toml
[notifications]
enabled = true
build_failures = true
deployment_failures = true
service_degradation = true
daemon_failures = true
recovery = true
failure_threshold_seconds = 120
retry_window_seconds = 86400

[[notifications.destinations]]
name = "operations"
kind = "json" # or "discord"
url = "https://receiver.example/private-webhook"
enabled = true
```

Build/deployment failures notify once until a successful operation clears that
condition. Service degradation is observed at application health level. Dependency
and service failures must remain continuously observed for the configured
threshold; stale observations and process downtime do not count toward it.
Internal daemon errors notify immediately, grouped by stable failure code.
Recovery notifications apply to observed dependencies/services and successful
operations that clear an alerted condition. Internal error groups without a
positive recovery observation stay deduplicated; they do not generate speculative
recovery messages or periodic reminders.

Those internal daemon groups retain one original event per failure code while
open, even with age pruning enabled. A quiet period or restart is not treated as
proof of recovery and does not re-arm their notifications.

Recovery is paired with failure deliveries at each destination, including its
configured URL identity. A new or changed destination never receives recovery for
a failure it was not sent. Recovery waits until all paired failure deliveries
finish retrying and at least one was acknowledged. If none was acknowledged,
recovery is cancelled. These relationships survive restart and protect the
failure history while recovery remains queued. The recovery retry window starts
with its first delivery attempt, so waiting for a failure cannot exhaust it.
Manual retry cannot replay a failed alert after its recovery was acknowledged.

The outbox is durable and separate from runtime work. JSON payloads contain
`version: 1`, `instance_id`, `delivery_id`, `category`, and the source `event`.
Discord messages include diagnostic context and disable mentions. Every request
carries `Idempotency-Key: <delivery_id>`; delivery is at least once, so receivers
should deduplicate. Retries preserve that ID through restart and manual retry.

Destinations require HTTPS; HTTP is allowed only for localhost or loopback IP
addresses. Requests time out after ten seconds and do not follow redirects. Transport
failures, HTTP 408/429 and 5xx retry with exponential backoff, capped at one hour,
within the configured window (24 hours by default). Integer `Retry-After` values
are honored up to one hour. Other non-2xx responses fail permanently. Manual retry
starts a new retry window while preserving identity, creation time and attempt
count. Delivery history exposes safe failure summaries, never response bodies.

Disabling a category/destination or changing its URL cancels pending deliveries
at startup. Re-enabling activates new events only, without replaying historical
failures. Deletion removes application-owned queued deliveries; a request already
in flight cannot be unsent. Copies already delivered to external systems are
outside piqueld's deletion policy.

## Optional metrics and future external services

```toml
[metrics]
listen = ["127.0.0.1:9464"] # default [] disables the listener
```

This separate listener serves only `GET /metrics`, in Prometheus text format. It
never serves the dashboard or administrative API. Measurements use fixed metric
names and no application/operation/diagnostic labels. Bind to an address reachable
by the intended scraper; loopback is only reachable in the host network namespace.
An enabled listener has no built-in authentication or TLS, so choose its exposure
accordingly. Its lifetime is supervised with the daemon's API listeners.

A future user-deployed Prometheus/collector/Grafana application needs:

1. Container image services and persistent named volumes for external storage.
2. A reachable scrape endpoint, or host access for a separately operated log
   collector. Docker service/build log APIs are bounded snapshots, not an external
   log-shipping protocol.
3. Monitoring configuration and Grafana provisioning, either baked into images or
   supplied through a future file/config mounting feature.
4. Network reachability among its services and browser access to Grafana. Published
   ports and stable service aliases would make this straightforward; otherwise
   users must arrange access and use available runtime service names themselves.
5. External retention, dashboards, rules and credentials managed by that
   application/operator.

piqueld does not need vendor-specific lifecycle integration or an OTLP destination
for these services. There is intentionally no example application manifest yet.
