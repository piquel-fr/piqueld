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

Interactive `apply` and `delete` require a TTY confirmation unless `--yes` is
provided. `--yes` is the explicit noninteractive confirmation for scripts.
Deleting an application retains its named volumes; the CLI prints that notice
and includes `volumes_retained: true` in JSON output.

Apply durably accepts intent before image preparation. Identical manifests do
nothing unless the operation failed; failed operations retry under the same ID.
`reconcile` repairs the latest intent using stored digests. `refresh` explicitly
resolves image references again and is rejected during deletion. Both accept
`--yes` and `--no-wait` like apply/delete.

`plan`, `apply`, `delete`, `reconcile`, and `refresh` accept an optional
`--expected-generation N`. Checks are opt-in; zero on apply means create-only.
A changed manifest or deletion intent advances generation. Image refresh does
not. `show` reports intent and resolved target generations and separate observed
runtime health; a resolved target does not imply container convergence.

The CLI retains one automatic transport retry and sends no idempotency keys.
A retry after a lost success response can encounter a generation conflict if a
precondition was supplied. A refresh retry after completion can start a new
refresh. There is no exact request replay guarantee.

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
