# Observability

piqueld keeps structured control-plane history in SQLite. Operation records are
execution state; events are the independently readable historical record. Metrics
exposure is optional. There is no OTLP destination, external log exporter,
collector management, or external daemon-outage monitoring in this implementation.

## History and diagnostics

Saved typed edits record their field and resource once, in the application's
history with no environment, without copying configuration values.

Secret writes and deletions record `secret_saved` and `secret_deleted` events with
the logical name as their resource; history never contains values. Removing a
secret's Docker versions is a journaled `remove_secrets` action, whether it follows
an API deletion (environment-owned, without an operation) or environment deletion.
A missing, unreadable or non-matching master key produces a daemon-scoped
`secret_storage_unavailable` diagnostic whose causal fact names the key condition.
Deployments decrypt values before their service request, so this failure is never
reported as a Docker error. Lost-key recovery records a daemon-scoped
`secret_key_recovered` event with value counts, and `secret_values_discarded` in
each affected environment's history.

Commands run with `app exec` record `command_started`, naming the account and
task, and `command_finished` with its exit code. Their resource is the logical
service. Commands are never recorded because their arguments can contain secrets.

An operation groups execution attempts; an attempt groups actions. Action events
carry an action ID, operation ID, environment ID, generation, attempt, phase,
resource, request/retry number, and completed duration where applicable. Source
preparation and runtime mutations, including managed ingress gateway changes,
commit intent before executing. Each mutating request commits an
`action_requested` event before calling Docker or Caddy. Failure to
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
API errors, not persisted incidents. Daemon logs include occurrence IDs. API
storage failures (`storage_unavailable`, `schema_mismatch`) are only logged, so
their response ID correlates with daemon logs but is not retrievable from the
database. Other diagnostics are likewise log-only if their write fails.

API failure occurrences are not sampled or deduplicated: repeated failed requests,
including dashboard polling during an outage, each retain their request ID and
diagnostic. Health and reconciliation observations of the same environment and
failure code share one diagnostic within a discovery pass. Separate passes retain
their own occurrences. Use nonzero retention settings to bound historical growth.

Safe causal facts include Docker request stages and HTTP statuses, I/O error
kinds and OS codes, command stages and exit codes, database failure kinds/codes,
and validated compilation diagnostics. Preparation and execution use the same
extraction rules; arbitrary source messages and command output stay excluded.

Raw engine response bodies, manifest environment values, webhook URLs and receiver
response bodies are excluded from diagnostics. Build output remains a separate,
bounded log and may contain text emitted by the build itself.

## Audit trail

The audit trail records who did what through the API: every refused request
(missing credentials or permission, a hidden application, a rejected origin,
or rate limiting), every write and sign-in, every command run with `app exec`
(and, separately, one refused when it starts because access was lost after
connecting), and reads of logs, the daemon configuration, manifest downloads,
and the account directory. Routine reads, such as dashboard polling of
application and environment views (whose saved configuration `apps:read`
already allows) and CLI logins still awaiting approval, appear only in daemon
logs. Records are written in the background so requests never wait for them;
at most 1,024 wait at once, of which anonymous requests may hold only half, and
on shutdown the daemon waits up to 10 seconds for pending records and logs any
it loses. Each record keeps the action
(method and route template), outcome (`allowed`, `denied`, or `failed`), status,
account, credential and its kind, whether the credential was limited, the
network address (and, through the [tailnet node](configuration.md#tailnet-node),
the tailnet user or tags and device, such as `alice@example.com on laptop`),
request ID, addressed application or environment (with the environment's
application, when the request was allowed or lacked a permission there, so a
refusal never reveals who owns an environment hidden from its caller), and the
permission a refusal lacked. Requests by the
[host operator](authentication.md#the-host-operator) have no account; they
record `operator` with its Unix user ID instead, shown as
`host operator (uid 0)`, and its browser session as the credential, if any. Bodies are never recorded. Records copy the account and
credential, so they outlive both, and application deletion keeps them.

`GET /api/v1/audit` lists the trail newest first, filtered by `user_id`,
`username` (as recorded, ignoring case, so deleted accounts' too), `credential_id`, and
`outcome`. Everyone reads their own account's trail, and API tokens and limited
CLI logins only their own requests; `audit:read` is needed for more. `piquelctl audit` and the dashboard's
Audit page read the same trail, and each session or token on the Accounts page
links to its activity. `retention.audit_days` (default 365, `0` disables
pruning) bounds it independently of other history.

Operations record the account and credential whose request created them
(including each environment's deletion when an application is deleted), and
every event about an operation carries them, including runtime actions long
after the request; each action keeps the actor it started under, even if
someone else restarts the operation meanwhile. Events written directly by a request carry them too: secret
writes and deletions (including the deletion's runtime action, even when a
restart interrupts it), environment changes, the start and end of commands
run with `app exec`, secret key recovery, and diagnostics for failed requests. Events show them as `actor_user_id` and `actor_credential_id`, or,
for the host operator, `actor_operator` with its Unix user ID and its browser
session (if any) as `actor_credential_id`; all are empty for the daemon's own
work.

The metrics listener exports `piqueld_access_denied_total`, the number of
refused API requests since the daemon started.

### Tamper evidence

Audit records have consecutive IDs, and each stores a link: the SHA-256 of the
previous record's link and its own ID and fields. Editing, inserting,
renumbering, or removing a record breaks the chain from there on.
`piquelctl audit verify`, the dashboard's **Verify integrity** button, and
`GET /api/v1/audit/verify` (all requiring `audit:read`) recompute the chain and
report the first record that does not match (the change may also be a removal
or insertion just before it). The command fails when the chain is broken.

```console
$ piquelctl audit verify
Chain: intact
Records checked: 1834
Pruned through: 1203 9a40…
Newest: 3037 6f1c…
```

Pruning by `retention.audit_days` removes the oldest records only while the
chain through them verifies, so it never erases evidence of tampering; the
newest pruned record becomes the anchor the remaining chain extends. The chain
detects changes to the database, but cannot tell who wrote it: someone who
controls the host could rewrite the whole trail, remove the newest records, or
prune more and move the anchor. Keep the reported anchor and newest record
somewhere piqueld cannot change, such as a ticket or a scheduled job's log, and
compare them later to detect that.

### Security notifications

The `security` notification category reports changes to who can access the
daemon, as daemon history events that each notify immediately:

| Event | When |
| --- | --- |
| `admin_granted` | An account receives `admin` on every application: by an account change, an invitation, first-account setup, or an admin recovery link |
| `privileged_token_created` | An API token is created without expiry or with `admin` |
| `admin_recovery_issued` | `piquelctl recover-admin` issued a recovery link; see [authentication](authentication.md#recovering-administrator-access) |
| `operator_sign_in_issued` | `piquelctl sign-in-link` issued a link that signs a browser in as the host operator; see [authentication](authentication.md#the-host-operator) |
| `access_denial_burst` | 20 requests from one address and account are refused within a minute; at most once every 10 minutes per address and account while it continues |
| `credential_new_address` | A session, CLI login, or token already used elsewhere is used from a new network address (IPv6 by /64) |
| `secret_key_recovered` | The secrets master key was recovered, discarding stored values |

## Ownership and retention

`scope` determines ownership, independently of a contextual `environment_id`:

- `application`: removed with application deletion, including diagnostic history
  and associated webhook deliveries. Deleting an environment removes its
  deployment and build history, while its events stay in the application's
  history.
- `daemon`: shared infrastructure or internal failures remain under daemon
  retention, even when they refer to an environment that has since been deleted.

`retention.event_days` and `retention.daemon_event_days` both default to 90 days
and independently bound the two scopes; zero disables age-based pruning. Executing actions, open notification incidents, and pending deliveries
protect their source events. An open incident retains its failure delivery history
until recovery, even when the failure was already acknowledged.
Build output retains its existing byte and age limits. Operation pruning does
not make event details unreadable. SQLite file size need not immediately shrink
when rows are deleted; free pages can be reused.

Analytics reports when an interval extends before detailed history began or
includes explicitly pruned history. Event-based aggregates cover retained
applications: a deleted environment's events stay in its application's history,
and deleting the application removes them. Event streaming detects
retention gaps conservatively across both scopes. Deleting an application's
history intentionally removes that data rather than exposing deletion tombstones.

## API and dashboard

Routes require the permissions described in [authorization](authorization.md):
application history, diagnostics, and analytics are limited to the applications a
caller holds `events:read` on, and daemon history requires `system:read`. See the generated
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

Event filters: `application_id` (application-wide events and those of all its
environments), `environment_id`, `operation_id`, `attempt`, `action_id`, `kind`,
`error_code`, `errors_only`, `scope`, `since_ms`, `until_ms`, and `descending`.
Pages contain at most 100 records and use opaque `v1:<id>` cursors. Oldest-first
ordering remains the default. Timestamps are Unix milliseconds.

SSE uses the same filters with oldest-first ordering. Set `Last-Event-ID` to the
last received SSE ID to reconnect (it takes precedence over `cursor`). Without a
cursor, replay starts at retained history. Events are read from the journal in
bounded pages; slow clients do not block writers. When the filter excludes newer
events, the stream sends an ID-only message so `Last-Event-ID` still advances. A stale resume position receives
HTTP 410, or a terminal `history_expired` event if pruning happens during the
stream. A terminal `stream_error` indicates a storage failure. Clients should
refresh their snapshot before reconnecting after an expired history response.

The dashboard adds Events, Errors, Analytics, Notifications and Daemon status.
Application pages include scoped history; diagnostics link to related operation
history. Error feedback links to the specific diagnostic where available. Views
refresh while visible. Configuration is displayed read-only, with destination
URLs omitted; only the file a URL was read from is shown.

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
security = true # access changes; see "Security notifications"
failure_threshold_seconds = 120
retry_window_seconds = 86400

[[notifications.destinations]]
name = "operations"
kind = "json" # or "discord"
url = "https://receiver.example/private-webhook"
enabled = true
```

Webhook URLs are credentials. Use `url_file` instead of `url` to read one from a
file, such as a systemd credential; see [configuration](configuration.md#credential-files).

Build/deployment failures notify once until a successful operation clears that
condition. Service degradation is observed at environment health level. Docker,
Swarm and managed ingress gateway health are dependency conditions under
`daemon_failures`; deployments failing because a dependency is unavailable do not
notify separately. Dependency and service failures must remain continuously
observed for the configured threshold; stale observations and process downtime do not count toward it.
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
Manual retry cannot replay a failed alert after its condition closes, even if no
receiver acknowledged that alert. Recovery deliveries remain manually retryable
until the same condition opens again.

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
failures. Deletion removes environment-owned queued deliveries; a request already
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
4. Browser access to Grafana. Services reach each other by manifest name on the
   application's private network (for example `prometheus:9090`); published
   ports would make external access straightforward, otherwise users must
   arrange it themselves.
5. External retention, dashboards, rules and credentials managed by that
   application/operator.

piqueld does not need vendor-specific lifecycle integration or an OTLP destination
for these services. There is intentionally no example application manifest yet.
