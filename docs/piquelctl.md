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

The server resolves images and deduplicates equivalent targets. The CLI does
not create idempotency keys or send generation checks. It retries a transport
failure once using the same manifest. Applying a mutable image tag again can
produce new work if the digest changed.

By default, `apply`, `delete`, and `operation` poll the accepted operation every
250 ms until it reaches a terminal state. `--no-wait` returns immediately.
Pressing Ctrl-C ends only the local wait; it does not cancel the server-side
operation, which can still be inspected with `piquelctl operation <id>`.

The commonly useful exit codes are 0 for success, 1 for a general error, 2 for
usage or input errors, 3 for conflicts, 4 for unavailable or timed
out requests, 5 for a failed operation, and 130 when local operation waiting is
interrupted.

There are no mutating browser controls. Logs, remote authentication, builds,
registry management, and advanced interactive CLI flows remain future work.
