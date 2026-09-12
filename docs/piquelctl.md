# `piquelctl`

`piquelctl` is the small operator client for piqueld. It uses the
public `piqueld-client` contracts and talks to the daemon over a Unix socket by
default.

## Commands

```console
piquelctl status
piquelctl list
piquelctl show <name-or-id>
piquelctl plan --file application.toml
piquelctl apply --file application.toml
piquelctl delete <name-or-id>
piquelctl operation <operation-id>
piquelctl reconcile <name-or-id>
piquelctl refresh <name-or-id>
piquelctl rename <name-or-id> <new-name>
piquelctl events --application <application-id> --limit 50
```

`--socket PATH` selects a Unix socket. `--url URL` selects an explicit loopback
HTTP origin such as `http://127.0.0.1:7845/`; the two transport options are
mutually exclusive. The default socket is
`/var/lib/piqueld/piqueld.sock`.

Global `--timeout DURATION` defaults to `30s`. Durations are positive integer
milliseconds (`ms`), seconds (`s`), minutes (`m`), or hours (`h`); a bare integer
is interpreted as seconds. The timeout bounds the complete command; interactive
confirmation prompts do not consume it. Read requests can use the full remaining
command budget. Idempotent mutations retry one transport failure if time remains;
a request that exhausts the command budget cannot be retried.
Manifest files must be regular UTF-8 files no larger than 2 MiB, matching the
daemon's API request body limit.

Use `--json` for machine-readable output. JSON is made only from public API
DTOs and the small CLI composition objects below; diagnostics and progress are
written to stderr, so stdout remains valid JSON.

| Command | JSON output |
| --- | --- |
| `status` | `SystemStatus` |
| `list` | `{ "items": [{ "application": ApplicationSummary, "status": ApplicationStatusView }], "next_cursor": null }` |
| `show` | `{ "application": ApplicationView, "status": ApplicationStatusView }` |
| `plan` | `PlanView` |
| identical `apply` | `{ "identical": true, "application_id": string, "outcome": OperationState, "operation": Operation }` |
| `rename` | `RenamedApplication` |
| `apply --no-wait` | `AcceptedOperation` |
| `apply` | `{ "accepted": AcceptedOperation, "outcome": OperationState, "operation": Operation }` |
| `delete --no-wait` | `{ "accepted": AcceptedOperation, "volumes_retained": true }` |
| `delete` | `{ "accepted": AcceptedOperation, "outcome": OperationState, "operation": Operation, "volumes_retained": true }` |
| `operation --no-wait` | `Operation` |
| `operation` | `Operation` |
| `reconcile` / `refresh` | `{ "accepted": AcceptedOperation, "outcome": OperationState, "operation": Operation }` |
| `reconcile --no-wait` / `refresh --no-wait` | `AcceptedOperation` |
| `events` | `{ "items": [Event], "next_cursor": string or null }` |

The DTO fields and error envelope are defined by the versioned API and the
`piqueld-client` crate. CLI errors are reported on stderr and never mixed into
JSON stdout.

## Mutation safety

`plan` previews a manifest, and `apply` sends it to the single apply endpoint.
The server creates or updates the application identified by its name. `apply`
always previews first and stops when the plan is blocked or confirmation is
declined. `show` and `delete` accept a name or ID; name lookup follows the
paginated application list.

Mutating commands require a TTY confirmation unless `--yes` is supplied.
Apply, delete, and rename also accept `--force` to override preconditions;
force does not skip confirmation. Unattended forced commands need both
`--force --yes`. `--force` and `--expected-generation` are mutually exclusive.
Deleting an application retains its named volumes; the CLI prints that notice
and includes `volumes_retained: true` in JSON output.

Apply durably accepts intent before image preparation. An identical ordinary
apply schedules no new work and needs no confirmation. It waits for the existing
pending/running operation unless `--no-wait` is supplied. An existing success
returns immediately and does not establish fresh runtime health. Failed/cancelled
operations exit with code 5 and guidance to use `reconcile`. A forced apply always
sends its request to the endpoint, even when the preview was identical, so the
server can apply it to the name's current intent.

`reconcile` repairs or retries latest intent using stored digests, including
continuing an already-requested deletion. Apply reuses active digests for unchanged
service image references; `refresh` explicitly resolves them again and is rejected
during deletion. Reconcile and refresh accept `--yes` and `--no-wait`; they do not
require a revision and have no `--force` flag.

Previews are computed by the daemon. They redact sensitive configuration values
and identify unresolved images separately from known runtime actions. If Docker
observation is unavailable, the daemon returns `503 docker_unavailable` and
`plan`/`apply` stop without submitting new intent.

For apply, delete, and rename, the CLI automatically sends the revision it
inspected before confirmation. Apply also sends the inspected application ID,
or generation zero for create-only. `--expected-generation N` supplies an explicit
revision for scripts. Conflicts stop the command without adopting the newer
revision. `--force` skips these preconditions: forced apply targets whichever
application currently has the name, or creates it if absent. Validation, resource
ownership, and rename busy/name-collision checks still apply. `show` reports intent
and active target generations and separate runtime health.

The CLI retains one automatic transport retry, using the same command UUID in
`Idempotency-Key`. SQLite receipts replay the original acceptance response for
24 hours across daemon restarts. Replays do not restart failed or superseded work;
a separately invoked command gets a new UUID. This includes forced requests:
a transport retry cannot overwrite changes accepted after the original command.

Rename checks the inspected generation and name availability. It rejects pending
or running operations and deletion intent. It preserves identity, services,
networks, and volumes without image resolution or redeployment. A changed name
advances generation and records an event. Update `metadata.name` in your manifest
file afterward; the CLI does not edit files automatically.

`events` reads one page, oldest first. `--application ID` optionally filters by
stable ID, including deleted applications. Use `--cursor CURSOR` for subsequent
pages and `--limit N` (1–100, default 50). JSON includes the next cursor.

By default, apply, delete, reconcile, refresh, and operation poll every 250 ms
until a terminal state. Supersession returns immediately with exit code 0 and
`outcome: "superseded"` in mutation command results (`state: "superseded"` on an
operation record). It does not wait for the replacement to deploy. An observed
failed attempt returns failure even if the controller will retry automatically.
`--no-wait` returns immediately. For long image pulls,
use a longer `--timeout` or return immediately and inspect the operation later.
Pressing Ctrl-C ends only the local wait; it does not cancel the server-side
operation, which can still be inspected with `piquelctl operation <id>`.

The commonly useful exit codes are 0 for success or supersession, 1 for a general error, 2 for
usage or input errors, 3 for conflicts, 4 for unavailable or timed
out requests, 5 for a failed operation, and 130 when local operation waiting is
interrupted.

There are no mutating browser controls. Logs, remote authentication, builds,
registry management, and advanced interactive CLI flows remain future work.
