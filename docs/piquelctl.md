# `piquelctl`

`piquelctl` is the operator client for the piqueld control plane. It uses the
public `piqueld-client` contracts and talks to the daemon over a Unix socket by
default. The original flat commands remain supported; grouped commands make
the larger workflow easier to discover.

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
piquelctl export --application <name-or-id> --output application.toml
piquelctl export --output state.tar --mode portable
piquelctl import state.tar

piquelctl application list|show|plan|apply|export|delete|logs ...
piquelctl secret list|set|delete ...
piquelctl operation watch <operation-id>
piquelctl state export|import ...
```

`--socket PATH` selects a Unix socket. `--url URL` selects an explicit loopback
or Tailscale HTTP origin such as `http://127.0.0.1:8080/` or
`http://100.64.0.10:8080/`; the two transport options are mutually exclusive.
The default socket is `/run/piqueld/piqueld.sock`.

Connection profiles may be selected with `--profile NAME` and
`--profiles-file PATH` (or `PIQUELD_PROFILE`, `PIQUELD_PROFILES_FILE`). The
default file is `$XDG_CONFIG_HOME/piqueld/profiles.toml`, falling back to
`$HOME/.config/piqueld/profiles.toml`. A profile contains one `unix_socket` or
`url` plus an optional protected `token_file` or `token_env`. Explicit
`--socket`, `--url`, `--token-file`, and `--token-env` options override the
profile. Tokens are sent as bearer credentials and are never included in
diagnostics or JSON output.

Global `--timeout DURATION` defaults to `30s`. Durations are positive integer
milliseconds (`ms`), seconds (`s`), minutes (`m`), or hours (`h`); a bare integer
is interpreted as seconds. The timeout bounds each client request and the
complete command.
Manifest files must be regular UTF-8 files no larger than 4 MiB.

Use `--json` for the legacy machine-readable output. `--output json` selects
the stable `piquelctl.v1` envelope; `--output human` selects human output.
Diagnostics and progress are written to stderr, so stdout remains valid JSON.

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
| `export --application` | Application manifest text, or a JSON summary with `--json` |
| `export` | Bounded binary state archive, or a JSON digest summary with `--json --output` |
| `import` | `StateImportResult` |

The DTO fields and error envelope are defined by the versioned API and the
`piqueld-client` crate. CLI errors are reported on stderr and never mixed into
JSON stdout.

The grouped commands cover the complete user-visible feature set:

- `application logs NAME` reads bounded, ANSI-sanitized logs; `--follow` uses
  the shared SSE cursor and reconnects with deduplication.
- `secret set NAME` reads from stdin or `--file PATH`; plaintext values are
  never command-line arguments, echoed from a terminal, or returned by list.
- `operation watch ID` follows operation events and build progress, then falls
  back to polling if the stream is unavailable. Ctrl-C stops only the local
  wait and does not cancel the daemon operation.
- `state export --file PATH` writes a portable or encrypted binary archive.
  `state import ARCHIVE --replace --yes` requires both explicit replacement
  intent and confirmation, verifies the archive digest, and uses the daemon's
  transactional import path.

Binary output is refused on a terminal. Existing files are not overwritten
unless `--force` is supplied, and binary output files are created privately.
Structured JSON cannot be mixed with binary stdout; use `--file` when both are
needed.

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
attached to an operation. Build logs remain deferred to later feature increments.

`logs` reads one bounded historical snapshot. It never follows or opens an SSE
stream. The grouped `application logs --follow` command owns the advanced
follow behavior.

Application exports are portable manifest text. Complete state exports are
bounded binary archives and can be portable or encrypted; binary output refuses
an interactive terminal. Import is transactionally confirmed and reports
missing secret values and retained volumes as explicit dependencies.

The commonly useful exit codes are 0 for success, 1 for a general error, 2 for
usage or input errors, 3 for generation conflicts, 4 for unavailable or timed
out requests, 5 for a failed operation, and 130 when local operation waiting is
interrupted.

Advanced grouped commands use stable exit categories: 0 success, 1 general
error, 3 input or validation, 4 authentication, 5 generation or state
conflict, 6 unavailable or timed out, 7 failed operation, 8 locally
interrupted wait, and 9 explicit refusal. Legacy flat `--json` commands retain
their Plan 06 exit mapping for compatibility; Clap usage errors use 2.

Profiles, authentication, stable structured output, operation watch, and log
follow are implemented here. Registries, routes, shell completion, editor
flows, and conflict merging remain deferred to later feature increments.
