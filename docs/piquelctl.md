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

The daemon runs on Linux. With its listen mode set to `tailscale` or `both`,
connect from a tailnet peer using its IP address or MagicDNS name:

```console
piquelctl --url http://linux-host:7845 status
```

For persistent configuration, set a named profile's `url` to
`http://linux-host:7845` and select it with `--profile`. See
[daemon configuration](configuration.md#tcp-listen-modes-and-tailscale).

Both platforms discover system and user profiles at runtime, including binaries
built with Cargo. See [connection profiles](#connection-profiles).

## Commands

```console
piquelctl profiles
piquelctl status
piquelctl app list
piquelctl app show <name-or-id>
piquelctl app logs <name-or-id> [--service <name>]
piquelctl app plan --file application.toml
piquelctl app apply --file application.toml
piquelctl app apply --file application.toml --deploy
piquelctl app delete <name-or-id>
piquelctl operation <operation-id>
piquelctl app reconcile <name-or-id>
piquelctl app deploy <name-or-id>
piquelctl app rename <name-or-id> <new-name>
piquelctl events --application <application-id> --limit 50
```

`--socket PATH` selects a Unix socket. `--url URL` selects an explicit
HTTP origin such as `http://127.0.0.1:7845/`; the two transport options are
mutually exclusive. The default socket is
`/run/piqueld/piqueld.sock`.

Remote IP addresses and DNS names are accepted. HTTPS, credentials, non-root
paths, queries, and fragments are rejected. DNS resolution and connection setup
share the request timeout; redirects are not followed. The client does not
verify that a destination belongs to Tailscale: HTTP outside a protected network
is unencrypted.

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
| `profiles` | `{ "profiles": [{ "name": string, "endpoint": string }] }` |
| `status` | `SystemStatus` |
| `app list` | `{ "items": [{ "application": ApplicationSummary, "status": ApplicationStatusView }], "next_cursor": null }` |
| `app show` | `{ "application": ApplicationView, "status": ApplicationStatusView }` |
| `app logs` | `ApplicationLogs` |
| `app plan` | `PlanView` |
| `app create` / `app rename` / field edits | `SavedApplication`; `--deploy` uses the same output as `app apply --deploy` |
| `app manifest` | Saved TOML as a JSON string |
| `app apply` | `SavedApplication` with null `operation_id` |
| `app apply --deploy --no-wait` | `SavedApplication` with a deployment operation ID |
| `app apply --deploy` | `{ "saved": SavedApplication, "outcome": OperationState, "operation": Operation }` |
| `app delete --no-wait` | `{ "accepted": AcceptedOperation, "volumes_retained": true }` |
| `app delete` | `{ "accepted": AcceptedOperation, "outcome": "deleted", "volumes_retained": true }` |
| `operation --no-wait` | `Operation` |
| `operation` | `Operation` |
| `app reconcile` / `app deploy` | `{ "accepted": AcceptedOperation, "outcome": OperationState, "operation": Operation }` |
| `app reconcile --no-wait` / `app deploy --no-wait` | `AcceptedOperation` |
| `events` | `{ "items": [Event], "next_cursor": string or null }` |

The DTO fields and error envelope are defined by the versioned API and the
`piqueld-client` crate. CLI errors are reported on stderr and never mixed into
JSON stdout.

## Connection failures

Use `piquelctl status` to check whether the selected endpoint responds with the
piqueld API. Connection diagnostics are available on failures from every command;
there is no separate diagnostic command.

Failures identify the effective endpoint and its source (flag, environment,
profile and file, or built-in default), preserve the observed cause, and suggest
a check. Timeout failures also show the effective timeout and its independent
source. For example:

```text
Error: piqueld API request failed: opening connection: No such file or directory (os error 2)
  Endpoint: Unix socket /tmp/piqueld.sock
  Endpoint source: flag --socket
  Hint: Check the socket path and whether the daemon has created its socket.
```

Configuration failures identify the source that needs attention; they do not
claim an effective endpoint before resolution succeeds. Rejected URLs and TOML
source excerpts are omitted because they can contain credentials. Malformed
profile files report a location and a safe error category instead.

These diagnostics use the original request and resolved configuration. They do
not probe other targets, inspect permissions, or check database, Docker, or Swarm
readiness. Unexpected HTTP responses and invalid API data include connection
context; ordinary application errors keep their existing reporting. Diagnostics
remain human-readable on stderr with `--json` or `--quiet`, and exit codes and
successful output are unchanged.

## Field editing

Application commands now live under `piquelctl app`. The old top-level application
commands have been removed. `status`, `profiles`, `operation`, `events`, and
`builds` remain top-level. Manifest import (`app apply`) and preview (`app plan`)
remain available, but ordinary editing sends only the selected setting.

```console
piquelctl app create notes --yes
piquelctl app service add notes web nginx:stable --yes
piquelctl app service replicas notes web 3 --yes
piquelctl app service env set notes web RUST_LOG debug --yes
piquelctl app service env remove notes web RUST_LOG --yes
piquelctl app service command notes web --yes -- /usr/bin/server
piquelctl app service arguments notes web --yes -- --listen "0.0.0.0:8080"
piquelctl app service add notes worker --git https://example.com/app.git --branch main --yes
piquelctl app service source git notes web https://example.com/app.git --branch main --yes
piquelctl app service source branch notes web release --yes
piquelctl app service source commit notes web --clear --yes
piquelctl app service source dockerfile notes web build/Dockerfile --yes
piquelctl app service source context notes web build --yes
piquelctl app service source image notes web nginx:stable --yes
piquelctl app service cpu notes web 500 --yes
piquelctl app service memory notes web 268435456 --yes
piquelctl app service cpu notes web --clear --yes
piquelctl app volume add notes data --yes
piquelctl app service mount set notes web data /var/lib/data --yes
piquelctl app service mount remove notes web /var/lib/data --yes
piquelctl app volume remove notes data --yes
piquelctl app service health http notes web 8080 --path /live --check-timeout 3 --yes
piquelctl app service health interval notes web 20 --yes
piquelctl app service health clear notes web --yes
piquelctl app service replicas notes web 2 --deploy --yes
piquelctl app manifest notes
```

Every edit saves to the daemon's internal manifest by default. `--deploy` saves
and captures a deployment atomically, then waits for it; add `--no-wait` to return
its receipt immediately. No local manifest file is read or rewritten. The server
validates the complete result, so removing a mounted volume is rejected until its
mounts are removed. Removing declarations retains Docker volume data.

Command and argument arrays preserve individual shell arguments; place command
options before `--`. An empty array clears the setting. Optional limits and pinned
commits use `--clear`; health checks use `health clear`. Nested Git/health settings
require the corresponding source/check to be configured first. Use `--help` on
any command for its values and options.

Git-backed applications keep services, volumes, and the application name under
repository ownership. Connection settings remain editable. Disconnect explicitly
to retain the saved manifest and edit it locally:

```console
piquelctl app repository connect notes https://example.com/infra.git infra/app.toml --yes
piquelctl app repository branch notes release --yes
piquelctl app repository path notes corrected/app.toml --yes
piquelctl app repository disconnect notes --yes
```

## Mutation safety

`app apply` saves configuration only. `app apply --deploy` saves and deploys atomically.
Both inspect the application identity and saved revision before confirmation;
neither requires a runtime preview or Docker availability. Use `app plan` separately
to inspect redacted changes and image resolution requirements. Preview may fail
when Docker observation is unavailable.

Mutating commands require TTY confirmation unless `--yes` is supplied. Apply,
delete, rename, and field edits accept `--force` independently of confirmation. Every explicit
deployment creates a new snapshot, refreshes image references and supersedes pending
work. Completed deployments retain their terminal state in history.
Reconciliation retries the deployment snapshot using prepared digests.
Unchanged healthy containers do not restart unnecessarily.

Deletion removes application configuration and all its history after runtime
resources are absent. Named Docker volumes remain; output includes
`volumes_retained: true`. The CLI waits for application absence because deletion
also removes its operation record.

For apply, delete, rename, and field edits, the CLI automatically sends the revision it
inspected before confirmation. Apply also sends the inspected application ID,
or generation zero for create-only. `--expected-generation N` supplies an explicit
revision for scripts. Conflicts stop the command without adopting the newer
revision. `--force` skips these preconditions: forced apply targets whichever
application currently has the name, or creates it if absent. Validation, resource
ownership, and rename busy/name-collision checks still apply. `app show` reports intent
and active target generations and separate runtime health.

The CLI retains one automatic transport retry, using the same command UUID in
`Idempotency-Key`. SQLite receipts replay the original acceptance response for
24 hours across daemon restarts. Replays do not restart failed or superseded work;
a separately invoked command gets a new UUID. This includes forced requests:
a transport retry cannot overwrite changes accepted after the original command.

Rename checks the inspected generation and name availability. It rejects pending
or running operations and deletion intent. It preserves identity, services,
networks, and volumes. It saves without deployment unless `--deploy` is given. A changed name
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
authentication, registry management, and advanced interactive CLI flows remain future work.

`app deploy` fetches repository-backed configuration when configured, then explicitly
resolves image or Git build sources. It supersedes pending work for the selected
application. Use `--yes` to skip interactive confirmation, `--no-wait` to return
after acceptance, or a longer global `--timeout` for builds. The server continues
deployment if the CLI wait times out. `refresh` resolves only the stored service
sources; `app reconcile` retries or repairs the latest deployment snapshot and prepared target.

Human output uses bold labels and color on terminals, application lists,
and elapsed operation progress. Redirected output stays plain and `NO_COLOR`
disables color. `--quiet` suppresses human results, information, and progress while
preserving warnings, errors, and explicit JSON results. `--noninteractive` refuses prompts
even on a terminal; destructive commands still need `--yes`.

Output is routed through channels configured once at startup:

| Role | Stream | Quiet behavior |
| --- | --- | --- |
| Result | stdout, human or one typed JSON document | Human hidden; JSON retained |
| Information | stderr, routine guidance | Hidden |
| Warning | stderr, incomplete results or actionable caveats | Retained |
| Progress | stderr, task transitions and outcomes | Hidden |
| Prompt | stderr, an authorized interactive question | Retained |
| Error | stderr, final failure with context and hints | Retained |

Every event is rendered and flushed as it occurs; output is not collected until
the command ends. Terminal progress uses independently tracked task rows and
permanent completion lines. Redirected progress prints meaningful transitions,
without repeated identical updates. JSON remains a single result document, not
a progress stream; stderr stays human-readable in JSON mode. Paginated results
can collect typed records before that document is emitted. If an application's
status cannot be fetched during `app list`, its status is `null` and a contextual
warning is emitted immediately; other applications are still returned.

Dynamic human-readable values escape terminal control characters. Logs preserve
newlines and tabs, but escape other controls. Build logs additionally use the
shared ANSI/control cleanup before human rendering. JSON retains original values.
Result, information, warning, and prompt write failures fail the command;
progress and final error reporting are best effort.

Internally, command handlers emit typed `Report` values through one `Console`;
they do not branch on `--json` or `--quiet`. Reports define their JSON type and
stream human rendering through `HumanWriter`. Contextual errors and warnings
share a borrowed diagnostic renderer. Task handles may be cloned across workers,
but ordinary console writes remain serialized. Only explicit task completion
prints an outcome; dropping the last unfinished handle clears its transient row
without claiming the server-side operation was cancelled.

`piquelctl app logs NAME_OR_ID [--service NAME] [--tail 200] [--since-seconds 3600]`
reads a recent Docker snapshot with timestamps, service, task, and stream labels.
`--json` returns the structured records and a truncation indicator.

## Connection profiles

Select a named connection with `--profile NAME` or `PIQUELD_PROFILE`.
On Linux and macOS, the CLI loads `/etc/piqueld/profiles.toml`, then
`$XDG_CONFIG_HOME/piqueld/profiles.toml` (otherwise `$HOME/.config/piqueld/profiles.toml`).
User profiles replace entire system profiles with the same name; fields are not
merged. Other system profiles remain available. Discovery happens at runtime,
so installed and development binaries use the same configuration.

`--profiles-file PATH` takes precedence over `PIQUELD_PROFILES_FILE`. Either
explicit selection loads only that file, bypassing system and user discovery.
Missing automatically discovered files are ignored. Explicitly selected missing
files and all unreadable or malformed files are errors, with the file path.
Surviving profiles are validated after replacement; invalid entries report their
name and source file, even when they are not selected.

An optional `[profiles.default]` is used when no name is selected; an explicitly
selected missing profile is an error.

```toml
[profiles.testing]
socket = "/tmp/piqueld-dev-run/piqueld.sock"
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

`piquelctl profiles` lists the effective profile names and endpoints in alphabetical
order after file selection and replacement. It does not contact a daemon or check
endpoint reachability, and ignores `--profile`, `PIQUELD_PROFILE`, and connection
overrides. Only configured profiles appear; the built-in local connection is not
an additional profile.

```text
NAME  ENDPOINT
dev   /tmp/piqueld-dev/piqueld.sock
prod  http://127.0.0.1:7845
```

`piquelctl profiles --json` returns
`{"profiles":[{"name":"dev","endpoint":"/tmp/piqueld-dev/piqueld.sock"}]}`.
An empty list prints `No profiles configured.` in human mode, or
`{"profiles":[]}` in JSON.
`--quiet` suppresses human results, information, and progress, but preserves JSON,
warnings, errors, and authorized prompts.

`piquelctl builds list [--application ID] [--cursor CURSOR]` lists one page of build
attempts. `piquelctl builds logs ID [--before BYTE_OFFSET]` reads the newest bounded
output page, or an older page before the supplied cursor. It prints the cursor
for loading older output when available. Both support `--json`.

Application secrets are write-only:

```sh
piquelctl app secret notes list
piquelctl app secret notes set database-password --file ./password --yes
printf '%s' 'new-value' | piquelctl app secret notes set database-password --stdin --yes
piquelctl app secret notes delete database-password --yes
```

Prefer protected files or a secure stdin producer over literal shell values in
real use. `--expected-generation` pins a write to inspected metadata; otherwise
the CLI reads the current generation before confirming. `--json` returns only
metadata. Replacement creates a new version for a later Deploy and does not
change running deployments. Deletion refuses saved or runnable references.

Daemon-wide storage key replacement is separate from application secret rotation:

```sh
# Preserve all values and versions; no redeployment required.
piquelctl secrets replace-key --yes

# Lost-key recovery: discard stored values across ALL applications.
piquelctl secrets replace-key --discard-values --yes
```

Both commands require confirmation (`--yes` for automation) and report affected
application, secret and version counts. Default replacement fails if the old key
or any retained ciphertext cannot be authenticated; it never falls back to
recovery. Recovery leaves running Docker services alone. `app secret APP list`
marks discarded current values as unavailable. Set replacement values using the
same names, then Deploy explicitly. Existing deployment pins remain unavailable.
Back up the new key together with the database after replacement.

The CLI retries transport failures with the same idempotency key. An error or
interrupted command can still follow a committed replacement: inspect metadata
before starting another destructive recovery command, especially if someone has
already entered replacement values. Increase `--timeout` for long active rollouts
or large secret stores; key replacement waits for deployments to finish.
