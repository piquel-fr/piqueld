# `piquelctl`

`piquelctl` is the small operator client for piqueld. It uses the
public `piqueld-client` contracts and talks to the daemon over a Unix socket by
default.

## macOS support

On Apple Silicon, Nix users can run `nix build .#cli` or
`nix run .#cli -- --help`. On macOS the default flake package is also the CLI;
daemon and dashboard packages are Linux
only. `nix develop` provides the CLI development tools, and `just validate-cli`
lints and tests the CLI and its shared client/core crates. `just build-cli`
builds the release binary. CI runs these checks on `macos-latest`, verifies the
Nix package, and uploads an Apple Silicon CLI binary.

The daemon still runs on Linux. To reach it from a Mac, forward its loopback HTTP
port over SSH (using the port configured on your host):

```console
ssh -N -L 7845:127.0.0.1:7845 user@linux-host
# In another terminal:
piquelctl --url http://127.0.0.1:7845 status
```

Profiles use the same `$XDG_CONFIG_HOME/piqueld/profiles.toml` location on both
platforms, falling back to `~/.config/piqueld/profiles.toml`.

## Commands

```console
piquelctl status
piquelctl list
piquelctl show <name-or-id>
piquelctl logs <name-or-id> [--service <name>]
piquelctl plan --file application.toml
piquelctl apply --file application.toml
piquelctl apply --file application.toml --deploy
piquelctl delete <name-or-id>
piquelctl operation <operation-id>
piquelctl reconcile <name-or-id>
piquelctl deploy <name-or-id>
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
| `logs` | `ApplicationLogs` |
| `plan` | `PlanView` |
| `rename` | `RenamedApplication` |
| `apply` | `SavedApplication` with null `operation_id` |
| `apply --deploy --no-wait` | `SavedApplication` with a deployment operation ID |
| `apply --deploy` | `{ "saved": SavedApplication, "outcome": OperationState, "operation": Operation }` |
| `delete --no-wait` | `{ "accepted": AcceptedOperation, "volumes_retained": true }` |
| `delete` | `{ "accepted": AcceptedOperation, "outcome": "deleted", "volumes_retained": true }` |
| `operation --no-wait` | `Operation` |
| `operation` | `Operation` |
| `reconcile` / `deploy` | `{ "accepted": AcceptedOperation, "outcome": OperationState, "operation": Operation }` |
| `reconcile --no-wait` / `deploy --no-wait` | `AcceptedOperation` |
| `events` | `{ "items": [Event], "next_cursor": string or null }` |

The DTO fields and error envelope are defined by the versioned API and the
`piqueld-client` crate. CLI errors are reported on stderr and never mixed into
JSON stdout.

## Mutation safety

`apply` saves configuration only. `apply --deploy` saves and deploys atomically.
Both inspect the application identity and saved revision before confirmation;
neither requires a runtime preview or Docker availability. Use `plan` separately
to inspect redacted changes and image resolution requirements. Preview may fail
when Docker observation is unavailable.

Mutating commands require TTY confirmation unless `--yes` is supplied. Apply,
delete and rename accept `--force` independently of confirmation. Every explicit
deployment creates a new snapshot, refreshes image references and supersedes pending
work. Completed deployments retain their terminal state in history.
Reconciliation retries the deployment snapshot using prepared digests.
Unchanged healthy containers do not restart unnecessarily.

Deletion removes application configuration and all its history after runtime
resources are absent. Named Docker volumes remain; output includes
`volumes_retained: true`. The CLI waits for application absence because deletion
also removes its operation record.

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
stable ID. Deleted applications have no retained history. Use `--cursor CURSOR` for subsequent
pages and `--limit N` (1–100, default 50). JSON includes the next cursor.

By default, apply with `--deploy`, delete, reconcile, deploy, and operation poll every 250 ms
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

The dashboard provides application management forms and recent application logs. Remote
authentication, build logs, registry management, and advanced interactive CLI flows remain future work.

`deploy` fetches repository-backed configuration when configured, then explicitly
resolves image or Git build sources. It supersedes pending work for the selected
application. Use `--yes` to skip interactive confirmation, `--no-wait` to return
after acceptance, or a longer global `--timeout` for builds. The server continues
deployment if the CLI wait times out. `refresh` resolves only the stored service
sources; `reconcile` retries or repairs the latest deployment snapshot and prepared target.

Human output uses bold labels and color on terminals, aligned application lists,
and elapsed operation progress. Redirected output stays plain and `NO_COLOR`
disables color. `--quiet` suppresses successful human output and progress while
preserving errors and explicit JSON results. `--noninteractive` refuses prompts
even on a terminal; destructive commands still need `--yes`.

`piquelctl logs NAME_OR_ID [--service NAME] [--tail 200] [--since-seconds 3600]`
reads a recent Docker snapshot with timestamps, service, task, and stream labels.
`--json` returns the structured records and a truncation indicator.

## Connection profiles

Select a named connection with `--profile NAME` or `PIQUELD_PROFILE`.
`--profiles-file PATH` / `PIQUELD_PROFILES_FILE` overrides
`$XDG_CONFIG_HOME/piqueld/profiles.toml` (otherwise `$HOME/.config/piqueld/profiles.toml`).
An optional `[profiles.default]` is used when no name is selected; an explicitly
selected missing profile is an error.

```toml
[profiles.testing]
socket = "/tmp/piqueld-dev/piqueld.sock"
timeout = "2m"

[profiles.local-http]
url = "http://127.0.0.1:7845"
```

Profiles contain exactly one `socket` or `url`, plus an optional `timeout`.
Precedence is explicit flags, then `PIQUELD_SOCKET` / `PIQUELD_URL` /
`PIQUELD_TIMEOUT`, then profile settings, then built-in defaults. A transport
override replaces the entire profile transport. Simultaneous environment socket
and URL values are rejected unless an explicit transport overrides them.
Authentication and credentials are not profile settings yet.

`piquelctl builds [--application ID] [--cursor CURSOR]` lists one page of build
attempts. `piquelctl build-logs ID [--offset BYTE_OFFSET]` reads one bounded output
page and prints the next offset when available. Both support `--json`.
