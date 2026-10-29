# Observability

piqueld keeps structured control-plane history in SQLite. Operation records are
execution state; events are the independently readable historical record. The
daemon can also push its traces, metrics and logs to an OpenTelemetry collector;
see [telemetry export](#telemetry-export). It does not manage collectors or
monitor its own outages from outside, but an
[example application](#observability-stack) deploys a collector with Prometheus,
Tempo, Loki and Grafana.

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

Image cleanup journals each removal as a daemon-scoped `remove_image` action
whose resource is the image ID, so its `action_succeeded` events list the
removed images and `action_failed` the ones it had to keep.

Commands run with `app exec`, `preview exec` or the dashboard's **Terminal** tab record `command_started`, naming the account and
task, and `command_finished` with its exit code, or that its stream failed
before it exited. Their resource is the logical
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
or rate limiting), every write and sign-in, every command run with `app exec`, `preview exec` or the
dashboard's **Terminal** tab
(and, separately, one refused when it starts because access was lost after
connecting), and reads of logs, the daemon configuration, manifest downloads,
and the account directory. Routine reads, such as dashboard polling of
application and environment views (whose saved configuration `apps:read`
already allows) and CLI logins still awaiting approval, appear only in daemon
logs. Records are written in the background so requests never wait for them;
at most 1,024 wait at once, of which anonymous requests (made by neither an
account nor the host operator) may hold only half; records beyond that are
dropped and logged. The next record written then follows a `failed` record with
status 503 and no caller, `audit records missing before this one: N`, counting
those dropped or whose write failed, so the trail shows the gap. On shutdown the daemon waits up to 10 seconds for pending records and logs any
it loses. Each record keeps the action
(method and route template; for account management, followed by the
requested change, e.g. `POST /api/v1/auth/manage set_grants "<account ID>" (apps:read)`), outcome (`allowed`, `denied`, or `failed`), status,
account, credential and its kind, whether the credential was limited, the
network address (and, through the [tailnet node](../reference/configuration.md#tailnet-node),
the tailnet user or tags and device, such as `alice@example.com on laptop`),
request ID, addressed application or environment (with the environment's
application, when the request was allowed or lacked a permission there, so a
refusal never reveals who owns an environment hidden from its caller), and the
permission a refusal lacked. Requests by the
[host operator](../accounts/recovery.md#the-host-operator) have no account; they
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
run with `app exec`, `preview exec` or the dashboard's **Terminal** tab, secret key recovery, and diagnostics for failed requests. Events show them as one `actor`, tagged by `kind`: `account` with
`user_id` and `credential_id`, `operator` with the host operator's Unix user
ID and its browser session (if any) as `session_id`, or, for deployments sync
starts on push, `system` with `actor` `sync:poll` or `sync:webhook`. `actor` is
absent for the daemon's other own work.

[Telemetry export](#telemetry-export) counts refused API requests as the
`piqueld.access.denials` metric.

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
newest pruned record becomes the anchor the remaining chain extends. It stops at
the oldest record still retained: a record written after the clock stepped back
stays until every record before it has expired, and one dated in the future,
written while the clock ran ahead, holds back every record after it until that
date has passed retention too. The chain
detects changes to the database, but cannot tell who wrote it: someone who
controls the host could rewrite the whole trail, remove the newest records, or
prune more and move the anchor. Keep the reported anchor and newest record
somewhere piqueld cannot change, such as a ticket or a scheduled job's log, and
compare them later to detect that.

### Security notifications

The `security` notification category reports changes to who can access the
daemon, and to where its notifications go, as daemon history events that each
notify immediately:

| Event | When |
| --- | --- |
| `admin_granted` | An account receives `admin` on every application: by an account change, an invitation, first-account setup, or an admin recovery link |
| `privileged_token_created` | An API token is created without expiry or with `admin` |
| `admin_recovery_issued` | `piquelctl recover-admin` issued a recovery link; see [recovering access](../accounts/recovery.md#no-administrator-can-sign-in) |
| `operator_sign_in_issued` | `piquelctl sign-in-link` issued a link that signs a browser in as the host operator; see [the host operator](../accounts/recovery.md#the-host-operator) |
| `access_denial_burst` | 20 requests from one address and account are refused within a minute; at most once every 10 minutes per address and account while it continues |
| `credential_new_address` | A session, CLI login, or token already used elsewhere is used from a new network address (IPv6 by /64) |
| `secret_key_recovered` | The secrets master key was recovered, discarding stored values |
| `notification_destination_changed` | A [notification destination](#notifications) was created, changed, disabled or deleted. A changed destination receives it only when it stays the same receiver |

### Daemon start notifications

The `daemon_started` notification category reports each start of the daemon,
including restarts for updates. Once its listeners are bound and Docker Swarm is
ready, the daemon records a `daemon_started` event in daemon history whose
resource is its version, and whose message names the previous version after a
change, such as `piqueld 0.2.0 started (previously 0.1.0)`. A start that fails
before then records no event. A missing notification alone does not prove that,
since notifications may be disabled or delivery may fail: check daemon history
for the event and the delivery history for its status.

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

Routes require the permissions described in [permissions](../accounts/permissions.md):
application history, diagnostics, and analytics are limited to the applications a
caller holds `events:read` on, and daemon history requires `system:read`. See the generated
[OpenAPI document](../../openapi-v1.json) for exact contracts.

| Route | Purpose |
| --- | --- |
| `GET /api/v1/events` | Cursor pagination and filtering |
| `GET /api/v1/events/stream` | Replay and follow committed history through SSE |
| `GET /api/v1/diagnostics/{id}` | Original event and details for an occurrence |
| `GET /api/v1/system/resources` | Cached process, storage and queue measurements |
| `GET /api/v1/analytics/deployments` | Deployment outcomes, retries, durations and failure codes |
| `GET /api/v1/notifications/destinations` | [Notification destinations](#notifications), without URLs; `system:read` or `notifications:manage` |
| `PUT /api/v1/notifications/destinations/{name}` | Create or replace a destination |
| `DELETE /api/v1/notifications/destinations/{name}` | Delete a destination |
| `POST /api/v1/notifications/destinations/{name}/test` | Send a test notification now |
| `GET /api/v1/notifications/deliveries` | Credential-free delivery history |
| `POST /api/v1/notifications/deliveries/{id}/retry` | Retry a failed delivery under current policy; `system:operate`. 409 `delivery_not_retryable` names why one cannot be |

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
Streams recheck the caller's access while open (see
[when access changes take effect](../accounts/permissions.md#when-access-changes-take-effect)): history it may no longer read
stops arriving, and a caller that lost access entirely receives a terminal
`access_ended` message whose data is the refusal's `ErrorBody`. Streams end
without a message when the daemon shuts down; reconnect with `Last-Event-ID`.

Read deployment analytics for the last 7 days:

```console
piquelctl analytics --since 7d
```

`--app` and `--env` limit them to one environment; see the
[CLI reference](../reference/cli.md). The dashboard adds Events, Errors,
Analytics, Notifications and Daemon status.
Application pages include scoped history; diagnostics link to related operation
history. Error feedback links to the specific diagnostic where available. Views
refresh while visible. Host settings are displayed read-only; credentials show
only the file they were read from.

Resource snapshots are cached for five seconds. They include process uptime,
resident memory and CPU, DB/WAL sizes, available disk, retained event and
diagnostic counts, retained build-output bytes, operation queues, and pending/failed deliveries. OS
measurements that are unavailable are null and omitted from exported metrics, not
zero.
These are current samples; piqueld does not persist a resource time series.
`piquelctl resources` and `piquelctl settings` show the measurements and the
host settings in a terminal; see [piquelctl](../reference/cli.md).

## Notifications

Notifications go to destinations: webhooks stored in the database and managed
through the API, `piquelctl notification`, or the dashboard's Notifications
page. Changes apply at once. For example, to send deployment failures about
`shop` to Discord, mentioning a role:

```console
piquelctl notification set ops --kind discord --url-file discord-url \
  --categories deployment_failures,recovery --apps shop \
  --message 'deployment_failures=<@&123456789012345678>'
piquelctl notification test ops
```

Each destination has a unique name, such as `ops`, a kind (`json` or
`discord`), a URL, and chooses what it receives:

- **Categories**: `build_failures`, `deployment_failures`,
  `service_degradation`, `daemon_failures`, `recovery`, `security` (see
  [security notifications](#security-notifications)), `daemon_started`
  (see [daemon start notifications](#daemon-start-notifications)) and
  `deployment_succeeded`, each successful deployment. A new destination
  receives all of them but `deployment_succeeded`. A deployment that clears a
  failure a destination was sent waits for that recovery: once the recovery
  is delivered the deployment is not sent too, and otherwise it is.
- **A message per category**, of 1–1000 characters, sent with each of its
  notifications. Discord sends it as the message text, so mentions in it, such
  as a role's `<@&ROLE_ID>`, notify; nothing else in a notification does.
- **Applications**: every application, including later ones, or a list, by
  ID so renaming one keeps it selected. Daemon notifications, such as daemon
  failures, security events and daemon starts, reach a destination whatever
  its applications.
- **Previews**: whether it receives notifications about previews; a new
  destination does not.

Listing destinations needs `system:read` or `notifications:manage`, and
changing them `notifications:manage`. URLs are credentials: they are stored encrypted with
the [secret master key](../reference/configuration.md#secret-storage) and never returned,
and lost-key recovery deletes the destinations. Deliveries go straight to the
destination. They never follow redirects or use a proxy set in the
environment (`HTTPS_PROXY`), which would see the URL.

Disabling a destination, removing a category, or narrowing its applications
or previews cancels the pending deliveries affected; recoveries still follow
the failures it was sent. Enabling it again, or adding a category, starts
after the latest event and never replays history. A new URL or kind makes it a new
receiver: its pending deliveries are cancelled, and it never receives the
recovery of a failure sent before. Deleting a destination cancels its pending
deliveries; delivery history keeps its name. A test notification is sent at
once, outside the delivery queue and history.

Build/deployment failures notify once until a successful operation clears that
condition. Service degradation is observed at environment health level. Docker,
Swarm and managed ingress gateway health are dependency conditions under
`daemon_failures`; deployments failing because a dependency is unavailable do not
notify separately. Dependency and service failures must remain continuously
observed for two minutes; stale observations and process downtime do not count toward it.
Internal daemon errors notify immediately, grouped by stable failure code.
Recovery notifications apply to observed dependencies/services and successful
operations that clear an alerted condition. Internal error groups without a
positive recovery observation stay deduplicated; they do not generate speculative
recovery messages or periodic reminders.

Those internal daemon groups retain one original event per failure code while
open, even with age pruning enabled. A quiet period or restart is not treated as
proof of recovery and does not re-arm their notifications.

Recovery is paired with failure deliveries at each destination, as long as it
keeps its URL and kind. A new or changed destination never receives recovery for
a failure it was not sent. Recovery waits until all paired failure deliveries
finish retrying and at least one was acknowledged. If none was acknowledged,
recovery is cancelled. These relationships survive restart and protect the
failure history while recovery remains queued. The recovery retry window starts
with its first delivery attempt, so waiting for a failure cannot exhaust it.
Manual retry cannot replay a failed alert after its condition closes, even if no
receiver acknowledged that alert. Recovery deliveries remain manually retryable
until the same condition opens again.

Each delivery keeps the names of the application, and of the environment or
preview, it is about as they were when it was queued, as its `subject`, so
notifications name them even after a rename or deletion.

Discord messages send the category's message as their text, and describe the
notification in an embed coloured by category, whose title starts with an
emoji, such as `❌ Deployment failed · shop / production`: red ❌ for build
and deployment failures and 🛑 for daemon failures, amber ⚠️ for service
degradation, green ✅ for recoveries and 🚀 for deployments, 🔐 for security
events, 🔄 for daemon starts and 🔔 for tests. Fields name the application,
environment or preview, and branch, with the cause and next step of a
failure. The title links to the dashboard at `auth.public_url`: the
diagnostic, else the environment or preview. Discord shows the event's time
in each reader's time zone; delivery and event IDs are left out.

The outbox is durable and separate from runtime work. JSON payloads contain
`version: 2`, `instance_id`, `delivery_id`, `category`, a `title` such as
`Deployment failed`, the `subject`, an object whose `application`,
`environment` and `branch` are null for daemon notifications and whose
`preview` is a boolean, the category's `message` (or null), and the source
`event`; test notifications have a null `category`.
Version 1 payloads had neither `title` nor `subject`. Payloads are not signed. Every request
carries `Idempotency-Key: <delivery_id>`; delivery is at least once, so receivers
should deduplicate. Retries preserve that ID through restart and manual retry.

Destinations require HTTPS; HTTP is allowed only for localhost or loopback IP
addresses. Private and tailnet hosts are allowed, so receivers can stay internal;
anyone with `notifications:manage` can therefore make the daemon send requests
to them. Requests time out after ten seconds and do not follow redirects. Transport
failures, HTTP 408/429 and 5xx retry with exponential backoff, capped at one hour,
for 24 hours. Integer `Retry-After` values
are honored up to one hour. Other non-2xx responses fail permanently. Manual retry
starts a new retry window while preserving identity, creation time and attempt
count. Delivery history exposes safe failure summaries, never response bodies.
Deliveries are sent one at a time, back to back while any is due. After a
downgrade, a category this version does not know is not delivered: destinations
do not list it, a queued delivery of it fails instead of holding up the rest, and
history leaves it out.

Deletion removes environment-owned queued deliveries; a request already
in flight cannot be unsent. Copies already delivered to external systems are
outside piqueld's deletion policy.

## Telemetry export

piqueld can push its own telemetry over OTLP/gRPC to a collector, such as the
OpenTelemetry Collector or Grafana Alloy:

```toml
[telemetry]
endpoint = "https://otel.example.com:4317" # or "http://host:4317", "unix:///run/otel/otlp.sock"
headers_file = "otlp-headers"              # optional
containers = true                          # default
```

Without an endpoint, nothing is exported there; an application can still
[receive telemetry](#receiving-telemetry-in-an-application) from the daemon.
`http://` sends in plain text, so use
it only on loopback or a private network. `headers_file` holds gRPC metadata sent
with every export, such as a collector token, as one `name: value` per line. It
is a [credential file](../reference/configuration.md#credential-files): relative paths
resolve against `$CREDENTIALS_DIRECTORY`, and only its path is ever shown.
Endpoints cannot carry credentials or a path. Configuration is read at startup;
restart after changing it.

Every daemon record carries the resource attributes `service.name = piqueld`,
`service.version`, and `service.instance.id`, the installation's ID (`instance_id`
in `piquelctl status`).

**Traces.** Each [operation](#history-and-diagnostics) is one trace: a root span
`operation <kind>` from its creation until it finishes (just `operation`, from its
last attempt, when its environment was deleted before it was exported, as a
finished deletion's is), a child span per
execution attempt, and below that a span per journaled action (builds as
`building_git`, jobs as `run_job`, rollouts as `ensure_service`, waits as
`wait_for_service`, and so on), timed by the duration the journal records.
Spans carry `piqueld.application`, `piqueld.environment`, the operation, attempt
and action IDs, the outcome, and the failure code as `error.type`. Daemon actions
outside an operation, such as image cleanup, are traces of their own. Trace and
span IDs are derived from the operation, attempt and action IDs, so an operation
that continues after a daemon restart stays one trace. Spans are sent when they
end; an operation retried later sends its root span again with its new end.
API requests are spans too, named after their route, such as
`GET /api/v1/environments/{id}`.

**Metrics**, collected every 30 seconds with cumulative temporality:

| Metric | Labels | Meaning |
| --- | --- | --- |
| `piqueld.deployments` | `application`, `environment`, `outcome` | Finished deployment attempts: `succeeded`, `failed`, `cancelled`, `superseded` |
| `piqueld.deployment.duration` | same | Their duration in seconds, for attempts that started |
| `piqueld.builds`, `piqueld.build.duration` | `application`, `environment`, `outcome` | Finished builds (`succeeded`, `failed`, `unknown`) and their duration |
| `piqueld.operations` | `state` (`running`, `queued`) | Operation queue depth |
| `piqueld.service.replicas` | `application`, `environment`, `service`, `state` (`desired`, `running`, `healthy`) | Replicas of every deployed service; health is `healthy` against `desired` |
| `http.server.request.duration` | `http.request.method`, `http.route`, `http.response.status_code` | API latency in seconds |
| `piqueld.access.denials` | | API requests refused for missing credentials or permission |
| `piqueld.uptime`, `process.memory.usage`, `process.cpu.time`, `process.cpu.utilization`, `piqueld.database.size` (`file`), `piqueld.disk.available`, `piqueld.events`, `piqueld.diagnostics`, `piqueld.build_output.size`, `piqueld.notification.deliveries` (`state`) | | The [daemon statistics](#api-and-dashboard) |

Prometheus translates the names, such as `piqueld_deployments_total` and
`piqueld_deployment_duration_seconds`.

**Logs.** The daemon's own log events, at the level `RUST_LOG` selects, and
every history event, with its kind (`piqueld.event.kind`), message, IDs and
failure code. Events of an operation or action link to its span. Failures are
`WARN`, everything else `INFO`.

### Container telemetry

With `containers = true`, the default, piqueld also exports the output and
resource usage of every container it runs: the tasks of manifest services, job
runs, and its own ingress containers (the gateway, the apps node joining the
tailnet, and `cloudflared`). piqueld is their only observer, so a collector
needs neither the Docker socket nor host mounts, and manifests still cannot
mount either. Set `containers = false` to export only the daemon's own
telemetry.

Container telemetry goes to [receiving services](#receiving-telemetry-in-an-application)
too. Without an endpoint, it is exported only while some environment
receives telemetry, starting from the present each time one does. A receiving service's own output reaches the endpoint but
no receiver, so a collector that logs what it receives never receives its own
output again.

Each container's records carry its own resource:

| Attribute | Value |
| --- | --- |
| `service.name` | The manifest service or job name; `piqueld-gateway`, `piqueld-tailscale` or `piqueld-cloudflared` for ingress |
| `piqueld.workload` | `service`, `job`, or `infrastructure` for ingress containers |
| `piqueld.application`, `piqueld.environment`, `piqueld.environment.id` | Its environment, for services and jobs |
| `piqueld.preview` | The preview's slug, for previews |
| `container.id`, `container.name` | The Docker container |
| `piqueld.task.id` | The Swarm task, for services and jobs |

**Logs.** Every line of stdout and stderr is a log record whose body is the line,
with `log.iostream` (`stdout` or `stderr`), Docker's timestamp, and terminal
escape sequences removed. Severity is left unset: piqueld does not guess it from
the text. Lines longer than 16 KiB are cut, and a container may send 200 lines
a second, in bursts of up to 2,000; lines over that are dropped and counted.
piqueld starts following a container as soon as Docker reports it starting, and
lists containers every 10 seconds besides, reading each container's output from
its start, so a short job's output is read even after it exits.

Delivery is at least once across daemon restarts: every 10 seconds, piqueld
saves the last line of each container that was sent, or dropped as described
below, and resumes from there, so lines sent just before a restart may be sent
again, but none still queued or written while piqueld was down are missed, as
long as Docker still has them. When export is first enabled, or enabled again
after `containers = false`, it starts from the present. Within one run, lines
are dropped like any other record when the queue is full or the collector fails.

**Metrics**, polled every 30 seconds with one-shot Docker stats for every running
container, with cumulative temporality and counters starting at the container's
creation:

| Metric | Labels | Meaning |
| --- | --- | --- |
| `container.cpu.time` | | CPU time used, in seconds |
| `container.cpu.usage` | | CPUs used on average since the previous poll; 1 is one full core |
| `container.memory.usage` | | Memory used, page cache included, in bytes |
| `container.memory.usage.limit` | | Its memory limit, or the host's memory without one |
| `container.network.io` | `network.interface.name`, `network.io.direction` (`receive`, `transmit`) | Network bytes |
| `container.disk.io` | `disk.io.direction` (`read`, `write`) | Block device bytes |

Polls share the daemon's limit on concurrent Docker observations, so they never
hold up reconciliation.

### Delivery and status

Export never holds up reconciliation or API requests. Records wait in bounded
queues, one per signal; a full queue drops new records, and a batch the
collector does not accept is dropped rather than retried, so an unreachable
collector costs one request per second at most. A collector that accepts no
export for a minute raises one `telemetry_export_failed` daemon diagnostic
(`dependency_health_changed`), notified under `daemon_failures` once it has
lasted `failure_threshold_seconds`, and a recovery event once exports succeed
again. Telemetry still queued at shutdown gets five seconds to be sent.

`piquelctl status` and the Daemon status page show each destination: its
endpoint, the file its headers came from, the last successful export, how many
records were dropped since the daemon started, and whether it is connected or
failing, and whether container telemetry is exported. They list the
[receiving services](#receiving-telemetry-in-an-application) of the
environments you can read too. The settings themselves appear read-only in the
configuration view.

piqueld doesn't serve a Prometheus scrape endpoint. Prometheus 3 receives
OTLP itself (over HTTP, with `--web.enable-otlp-receiver`), so the collector
can forward metrics to it with its `otlp_http` exporter.

### Receiving telemetry in an application

A collector deployed as a piqueld application, such as the OpenTelemetry
Collector, can receive the daemon's telemetry directly. The daemon runs on the
host and can't reach an overlay network, and piqueld doesn't publish ports, so
it exports over a Unix socket in a directory it shares with the collector,
as it does with the [ingress gateway](ingress.md).

1. Mark the collector's service in the manifest. At most one service per
   application may (`telemetry_receiver_duplicate`), with one replica
   (`telemetry_receiver_replicas`), and its rollouts stop the old collector
   before starting the new one:

   ```toml
   [[spec.services]]
   name = "collector"
   receives_telemetry = true
   [spec.services.source]
   type = "image"
   image = "otel/opentelemetry-collector-contrib:0.163.2"
   ```

   `piquelctl app service telemetry APP SERVICE on|off` and the service's
   Telemetry settings in the dashboard edit it too.

2. Grant the environment telemetry. This needs the `telemetry:grant`
   [permission](../accounts/permissions.md#telemetry), held only by `admin`,
   since the receiving service gets the telemetry of every application:

   ```console
   piquelctl env telemetry observability production on
   ```

   The environment's Overview has a Grant telemetry button. Previews
   are never granted; promoted environments can be, as the grant belongs to the
   environment, not to its releases.

piqueld then creates `<data_dir>/telemetry/<environment-id>/<service>/`, owned
by the daemon's user with mode `0700`, and bind-mounts it at
`/run/piqueld/telemetry` in the receiving service. That service runs as the daemon's user and group
instead of its image's user, so the daemon can connect to the socket the
collector creates there. Its collector must listen on
`/run/piqueld/telemetry/otlp.sock`, with an OTLP/gRPC receiver over `unix`:

```yaml
receivers:
  otlp:
    protocols:
      grpc:
        endpoint: /run/piqueld/telemetry/otlp.sock
        transport: unix
exporters:
  debug:
service:
  pipelines:
    traces: { receivers: [otlp], exporters: [debug] }
    metrics: { receivers: [otlp], exporters: [debug] }
    logs: { receivers: [otlp], exporters: [debug] }
```

Mount it with a [file](../reference/manifest.md#files), for example at
`/etc/otelcol-contrib/config.yaml`. The daemon exports to every granted
environment's receiving service alongside `[telemetry] endpoint`, which is
optional, with the same records, bounded queues and guarantees: it never
waits, connects lazily, and retries while the collector isn't listening yet.
When a receiver's socket refuses connections for a second, such as one a
crashed collector left behind, the daemon removes it, so the collector can
listen again when it restarts. A receiver failing for a minute raises the same `telemetry_export_failed`
diagnostic, attributed to its environment and notified under
`service_degradation`.

The mount follows the grant live, like [links](../reference/manifest.md#links):
it is never part of a deployment.

- **Revoking** the grant, or deleting the environment, stops export to its
  socket at once, with whatever was still queued for it.
- On its next pass, within seconds, the controller removes the mount and the
  user override, which restarts the service like any mount change. It then
  removes the directory, once no service mounts it.
- **Granting** works the same way: the controller adds the mount within seconds,
  and the daemon connects once the collector listens.
- Deploying a receiving service in an environment that isn't granted runs it
  without the mount, with a `telemetry_not_granted` warning in `app plan`,
  promotion plans and the deployment. A collector configured as above can't
  listen without the directory, so it restarts until the environment is
  granted.

Each receiving service has a socket of its own, so renaming it, or moving
`receives_telemetry` to another service, takes one deployment: the new service
listens while the old one still runs, the daemon exports to the new socket
within seconds of the deployment, and the old directory is removed with the
old service.

`env show` and the environment Overview show whether the environment is
granted, which service receives it, and whether the daemon is connected.

The receiving application sees the telemetry of **every** application: their
names, environments, operations, failures, and the daemon's own logs, which can
include error details. Grant it only to an application whose writers and
deployers you trust with that, as you would with `system:read`. Its receiving
service also runs as the daemon's user, which matters only if it escapes its
container: nothing of the daemon's beyond the socket directory is mounted.

## Observability stack

[`examples/observability/application.toml`](https://github.com/piquel-fr/piqueld/blob/main/examples/observability/application.toml)
is an application that stores piqueld's telemetry and lets you explore it: an
OpenTelemetry Collector receiving the daemon's telemetry, Prometheus, Tempo and
Loki storing its metrics, traces and logs, and Grafana on a private route, with
a data source for each. It is a small starting point, with no dashboards, that
you can extend.

The manifest holds every configuration file inline, so it deploys from the
file alone, without a repository. Edits you make to the application, such as
its hostname, stay until you apply the file again.

### Before you start

- Your instance serves [private routes](ingress.md#public-and-private-routes)
  on its tailnet, with a [DNS-01 provider](ingress.md#dns-01-certificates) for
  the zone of Grafana's hostname.
- You can create applications, change and deploy them, and run commands in
  them: `apps:create`, `apps:write`, `apps:deploy` and `apps:exec`. Granting
  the stack the daemon's telemetry also needs `telemetry:grant`, an
  installation-wide permission that no preset includes: `admin` holds it, or
  it can be granted on its own. See
  [permissions](../accounts/permissions.md#grants).
- `piquelctl` is signed in to your instance, on a machine with a checkout of
  piqueld's repository. Run the commands in this section from the checkout's
  root. Without a checkout, download `application.toml` and pass its path
  instead.

### Deploy it

1. Choose Grafana's hostname, such as `grafana.example.com`, and point it at
   the instance. Like any
   [private route](ingress.md#dns-for-private-routes), it needs `A` and `AAAA`
   records to the apps node's tailnet addresses, unless piqueld
   [manages the zone's records](ingress.md#managed-dns-records).

2. Save the manifest, then set your hostname in place of
   `grafana.example.com`, the manifest's placeholder:

   ```console
   piquelctl app apply --file examples/observability/application.toml
   piquelctl app variable set observability grafana_hostname grafana.example.com
   ```

   `app apply` creates the `observability` application, with its `collector`,
   `prometheus`, `tempo`, `loki` and `grafana` services. Both commands ask you
   to confirm. In the dashboard, the application's **Variables** tab sets the
   hostname too.

3. Grant the application's environment the daemon's telemetry, then deploy it:

   ```console
   piquelctl env telemetry observability production on
   piquelctl app deploy observability
   ```

   `app deploy` asks you to confirm, then waits until the deployment finishes.
   Grant the telemetry first: without it, the collector can't start, and the
   deployment fails. The stack now receives the telemetry of every
   application; see
   [receiving telemetry](#receiving-telemetry-in-an-application).

### Check that it works

`piquelctl status` shows the telemetry receiver connected. Open the URL that
`piquelctl env url observability` prints, from the tailnet, and sign in as
`admin` with the generated password:

```console
piquelctl app exec observability grafana -- cat /run/secrets/grafana-admin-password
```

Then open **Explore** in Grafana and pick a data source:

- **Prometheus** has the daemon's metrics, with `job="piqueld"`, and the
  resource usage of each container, labelled with its attributes, such as
  `piqueld_application`.
- **Loki** has the daemon's logs and every container's output. Query them by
  `service_name`, such as `{service_name="piqueld"}`.
- **Tempo** has the daemon's traces. Search them by service or by name.

Prometheus keeps metrics for 15 days and Tempo traces for 14, their images'
defaults. Loki keeps logs until you delete its volume. Grafana keeps nothing
between restarts, so the password stays the generated one; add a volume at
`/var/lib/grafana` to keep the dashboards you create.

### Send applications' telemetry to it

Other applications can send their own telemetry to the collector. Export it to
them, here `my-app`:

```console
piquelctl app export set observability collector --applications my-app --deploy
```

The application's **Links** tab in the dashboard edits exports too. Then import
the collector in each of them, and point its OpenTelemetry SDK at it:

```toml
[[spec.imports]]
application = "observability"
service = "collector"

[[spec.services]]
name = "web"
[spec.services.environment]
OTEL_EXPORTER_OTLP_ENDPOINT = "http://collector.observability:4317"
OTEL_EXPORTER_OTLP_PROTOCOL = "grpc"
```

The collector also accepts OTLP/HTTP on port 4318. Importing applications can
send any telemetry, including records that pass for the daemon's, so export it
only to applications you trust. A name no application holds is granted to
whichever application [takes it first](../reference/manifest.md#links).
