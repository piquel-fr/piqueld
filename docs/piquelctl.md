# `piquelctl`

`piquelctl` is the small operator client for the Plan 06 workflow. It uses the
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
piquelctl secret list
piquelctl secret set <name> --stdin
piquelctl secret set <name> --file <private-file>
piquelctl secret delete <name>
piquelctl build show <build-id>
piquelctl build operation <operation-id>
piquelctl logs <name-or-id> [--since-seconds N] [--tail N] [--max-bytes N]
```

`--socket PATH` selects a Unix socket. `--url URL` selects an explicit loopback
HTTP origin such as `http://127.0.0.1:8080/`; the two transport options are
mutually exclusive. The default socket is
`/run/piqueld/piqueld.sock`.

Global `--timeout DURATION` defaults to `30s`. Durations are positive integer
milliseconds (`ms`), seconds (`s`), minutes (`m`), or hours (`h`); a bare integer
is interpreted as seconds. The timeout bounds each client request and the
complete command.
Manifest files must be regular UTF-8 files no larger than 4 MiB.

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
| `apply` | `{ "accepted": AcceptedOperation, "operation": OperationView }` |
| `delete --no-wait` | `{ "accepted": AcceptedOperation, "volumes_retained": true }` |
| `delete` | `{ "accepted": AcceptedOperation, "operation": OperationView, "volumes_retained": true }` |
| `operation --no-wait` | `OperationView` |
| `operation` | `OperationView` |
| `secret list` | `{ "items": [SecretMetadata] }` |
| `secret set` | `SecretMetadata` (metadata only) |
| `secret delete` | `{ "deleted": true, "name": "..." }` |
| `build show` | `BuildView` (includes application and operation IDs) |
| `build operation` | `Page<BuildView>` |
| `logs` | `{ "items": [ContainerLogView] }` |

The DTO fields and error envelope are defined by the versioned API and the
`piqueld-client` crate. CLI errors are reported on stderr and never mixed into
JSON stdout.

## Mutation safety

`plan` and `apply` accept `--expected-generation N`; `delete` accepts the same
option. A name lookup follows the paginated application list and a syntactically
valid application ID is fetched directly. `apply` always plans first and will
not mutate when the plan is blocked or when confirmation is declined.

Interactive `apply` and `delete` require a TTY confirmation unless `--yes` is
provided. `--yes` is the explicit noninteractive confirmation for scripts.
Deleting an application retains its named volumes; the CLI prints that notice
and includes `volumes_retained: true` in JSON output.

Each mutating invocation creates one idempotency key and reuses it for the
single safe transport retry. The key is not regenerated during a retry.

By default, `apply`, `delete`, and `operation` poll the accepted operation every
250 ms until it reaches a terminal state. `--no-wait` returns immediately.
Pressing Ctrl-C ends only the local wait; it does not cancel the server-side
operation, which can still be inspected with `piquelctl operation <id>`.

Secret values are accepted only from a noninteractive stdin pipe or a private,
regular, symlink-free file. They are never accepted as command arguments,
printed in output, included in errors, or returned by the API. `secret list`
shows metadata and references only. `secret delete` requires confirmation (or
`--yes`) and refuses locally when the secret is still referenced by an
application service.

Build visibility is intentionally small: `build show` reports one durable build
and its owning application/operation, while `build operation` lists the builds
attached to an operation. Build logs and follow/watch behavior remain deferred
to later feature increments.

`logs` reads one bounded historical snapshot. It never follows or opens an SSE
stream; follow/watch and profile behavior remain later feature increments.

The commonly useful exit codes are 0 for success, 1 for a general error, 2 for
usage or input errors, 3 for generation conflicts, 4 for unavailable or timed
out requests, 5 for a failed operation, and 130 when local operation waiting is
interrupted.

Profiles and configuration files, authentication and tokens, builds,
registries, routes, logs, state transfer, SSE, shell completion, editor flows,
conflict merging, and elaborate stable exit categories remain deferred to the
later CLI plan.
