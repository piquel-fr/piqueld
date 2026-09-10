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
| `list` | `{ "items": [{ "application": ApplicationView, "status": ApplicationStatusView }], "next_cursor": null }` |
| `show` | `{ "application": ApplicationView, "status": ApplicationStatusView }` |
| `plan` | `PlanView` |
| identical `apply` | `{ "identical": true, "application_id": string, "operation": Operation or null }` |
| `rename` | `RenamedApplication` |
| `apply --no-wait` | `AcceptedOperation` |
| `apply` | `{ "accepted": AcceptedOperation, "operation": Operation }` |
| `delete --no-wait` | `{ "accepted": AcceptedOperation, "volumes_retained": true }` |
| `delete` | `{ "accepted": AcceptedOperation, "operation": Operation, "volumes_retained": true }` |
| `operation --no-wait` | `Operation` |
| `operation` | `Operation` |
| `reconcile` / `refresh` | `{ "accepted": AcceptedOperation, "operation": Operation }` |
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

Interactive `apply` and `delete` require a TTY confirmation unless `--force` is
provided. `--force` is the explicit noninteractive confirmation for scripts.
Deleting an application retains its named volumes; the CLI prints that notice
and includes `volumes_retained: true` in JSON output.

Apply durably accepts intent before image preparation. Identical manifests print
“This is identical to the existing manifest” and current operation status, then
exit without confirmation, mutation, or waiting. Failed/cancelled operations exit
with code 5 and guidance to use `reconcile`; pending operations report their ID and
exit successfully. `reconcile` repairs or retries latest intent using stored
digests. Apply reuses active digests for unchanged service image references;
`refresh` explicitly resolves them again and is rejected during deletion. Both
reconcile and refresh accept `--force` and `--no-wait` like apply/delete.

Previews are computed by the daemon. They describe manifest changes even when
Docker observation is unavailable, redact sensitive configuration values, and
identify unresolved images separately from known runtime actions.

The CLI automatically sends the revision it inspected before confirmation. Apply
also sends the inspected application ID, or generation zero for create-only.
`--expected-generation N` supplies an explicit revision for scripts. Conflicts
stop the command rather than adopting the newer revision. `--force` only skips
confirmation; it never bypasses generation checks. `show` reports intent and
active target generations and separate runtime health.

The CLI retains one automatic transport retry, using the same command UUID in
`Idempotency-Key`. SQLite receipts replay the original acceptance response for
24 hours across daemon restarts. Replays do not restart failed or superseded work;
a separately invoked command gets a new UUID.

Rename checks the inspected generation and name availability. It rejects pending
or running operations and deletion intent. It preserves identity, services,
networks, and volumes without image resolution or redeployment. A changed name
advances generation and records an event. Update `metadata.name` in your manifest
file afterward; the CLI does not edit files automatically.

`events` reads one page, oldest first. `--application ID` optionally filters by
stable ID, including deleted applications. Use `--cursor CURSOR` for subsequent
pages and `--limit N` (1–100, default 50). JSON includes the next cursor.

By default, apply, delete, reconcile, refresh, and operation poll every 250 ms
until a terminal state. `--no-wait` returns immediately. For long image pulls,
use a longer `--timeout` or return immediately and inspect the operation later.
Pressing Ctrl-C ends only the local wait; it does not cancel the server-side
operation, which can still be inspected with `piquelctl operation <id>`.

The commonly useful exit codes are 0 for success, 1 for a general error, 2 for
usage or input errors, 3 for conflicts, 4 for unavailable or timed
out requests, 5 for a failed operation, and 130 when local operation waiting is
interrupted.

There are no mutating browser controls. Logs, remote authentication, builds,
registry management, and advanced interactive CLI flows remain future work.
