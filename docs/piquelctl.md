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
piquelctl --allow-insecure-http --url http://linux-host:7845 login
piquelctl --allow-insecure-http --url http://linux-host:7845 status
```

For persistent configuration, set a named profile's `url` to
`http://linux-host:7845` and select it with `--profile` and `--allow-insecure-http`. See
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
piquelctl app validate --file application.toml
piquelctl app exec <name-or-id> <service> [--env <env>] [-i] [-t] -- <command>...
piquelctl app plan --file application.toml [--env <env>]
piquelctl app apply --file application.toml
piquelctl app apply --file application.toml --deploy
piquelctl app delete <name-or-id> [--environments <names>]
piquelctl operation <operation-id>
piquelctl app reconcile <name-or-id>
piquelctl app deploy <name-or-id>
piquelctl app rename <name-or-id> <new-name>
piquelctl app secret <name-or-id> list|set|access|delete
piquelctl env list <app>
piquelctl env create <app> <name> [--branch <branch> [--commit <sha>]]
piquelctl env branch <app> <env> <branch> [--commit <sha>]
piquelctl env show <app> [<env>]
piquelctl env rename <app> <env> <new-name>
piquelctl env delete <app> <env>
piquelctl env deploy <app> [<env>] [--branch <branch> | --commit <sha>]
piquelctl env reconcile <app> [<env>]
piquelctl env logs <app> [<env>] [--service <name>]
piquelctl env secret <app> [--env <env>] list|regenerate|delete
piquelctl events --application <application-id> --limit 50
```

An application owns the saved manifest; its environments deploy it, each with its
own deployments, status, volumes, generated secrets, routes, and network. Applications have
an environment named `production` from creation (existing applications were
migrated to one that kept their ID). `env` commands select an environment by name
or stable ID; `ENV` may be omitted only when the application has exactly one.
`app deploy`, `app reconcile`, and `app logs` act on the application's only
environment and fail with an "environment required" input error naming the
environments when it has several; they never pick one. With several
environments, `app delete` requires `--environments a,b` naming every one.
`env delete` keeps the application and retains the environment's named volumes.
Environment creation, renames and deletions are conditioned on the inspected
application revision like other mutations (`--expected-generation`, `--force`),
and advance it, so a command based on an earlier inspection fails instead of
acting on a renamed or deleted environment.

Each environment of a repository-backed application follows its own branch of
the application's repository; the repository URL and manifest path stay on the
application (`app repository url|path`). `env create --branch` chooses the
branch, by default the one `spec.manifest` names, and `--commit` pins a commit.
`env branch` points an existing environment at another branch, or pins or
unpins (without `--commit`) a commit; nothing is redeployed until its next
deployment. Both fail with `manifest_repository_required` for applications
without a repository. `env list` and `env show` report the source as `saved` or
as the branch, e.g. `main` or `main@<commit>`.

The manifest is shared; values that differ between environments come from
[manifest variables](application-manifest.md#variables). Environments that render
the same route hostname conflict with `hostname_conflict`, naming the sibling
environment that reserves it. `env rename` fails with `environment_configured`
while the manifest the environment deploys (for a repository-backed one, the one
last fetched from its branch) has a `[spec.environments.<name>]` block for the old
or the new name, since renaming would change which block applies: remove the
block, rename, then add it back under the new name. `env show` lists each
variable's value in that manifest, or that it has none, and says when a
repository-backed environment has fetched nothing yet.

`--socket PATH` selects a Unix socket. `--url URL` selects an explicit
HTTP or HTTPS origin such as `http://127.0.0.1:7845/`; the two transport options are
mutually exclusive. The default socket is
`/run/piqueld/piqueld.sock`.

Remote IP addresses and DNS names are accepted. HTTPS is supported; embedded URL
credentials, non-root paths, queries, and fragments are rejected. DNS resolution
and connection setup share the request timeout; redirects are not followed.
Remote HTTP authentication, including `login`, requires `--allow-insecure-http`
when using separate transport encryption such as Tailscale. The client does not
verify tailnet membership or add encryption with this flag. Prefer HTTPS, such as
a daemon's [tailnet node](configuration.md#tailnet-node) at
`https://piqueld.<tailnet>.ts.net`; loopback HTTP and Unix sockets need no opt-in.
When the daemon runs a tailnet node, `status` also reports its login state,
certificate expiry, and whether `auth.public_url` matches the node. It also lists
each [DNS provider](configuration.md#dns-providers) with its zones and health,
and each DNS-01 certificate with its hostnames, expiry and last error.
`piquelctl dns refresh` checks the providers' credentials and zones now instead
of at the next hourly discovery, then prints the same DNS lines.

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
| `dns refresh` | `DnsStatus` |
| `app list` | `{ "items": [{ "application": ApplicationSummary, "environments": [EnvironmentRow] }], "next_cursor": null }` |
| `app show` | `{ "application": ApplicationView, "environments": [EnvironmentRow] }` |
| `env list` | `[EnvironmentRow]`, where `EnvironmentRow` is `{ "environment": EnvironmentView, "status": EnvironmentStatusView or null }` |
| `env show` | `EnvironmentDetailView`, with `"stored": [StoredSecret]` |
| `env create` / `env rename` / `env branch` | `EnvironmentView` |
| `app logs` / `env logs` | `ApplicationLogs` |
| `app validate` | `{ "application": string }` |
| `app exec` | None; the command's raw output |
| `app plan` | `PlanView` |
| `app create` / `app rename` / field edits | `SavedApplication`; `--deploy` uses the same output as `app apply --deploy` |
| `app manifest` | Saved TOML as a JSON string |
| `app apply` | `SavedApplication` with null `operation_id` |
| `app apply --deploy --no-wait` | `SavedApplication` with a deployment operation ID |
| `app apply --deploy` | `{ "saved": SavedApplication, "outcome": OperationState, "operation": Operation }` |
| `app delete --no-wait` | `{ "deleted": DeletedApplication, "volumes_retained": true }` |
| `app delete` | `{ "deleted": DeletedApplication, "outcome": "deleted", "volumes_retained": true }` |
| `env delete --no-wait` | `{ "accepted": AcceptedOperation, "volumes_retained": true }` |
| `env delete` | `{ "accepted": AcceptedOperation, "outcome": "deleted", "volumes_retained": true }` |
| `operation --no-wait` | `Operation` |
| `operation` | `Operation` |
| `app`/`env` `reconcile` / `deploy` | `{ "accepted": AcceptedOperation, "outcome": OperationState, "operation": Operation }` |
| `app`/`env` `reconcile --no-wait` / `deploy --no-wait` | `AcceptedOperation` |
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
piquelctl app service depends-on notes web --yes -- postgres
piquelctl app service rollout notes worker --order stop-first --monitor-seconds 60 --yes
piquelctl app service add notes worker --git https://example.com/app.git --branch main --yes
piquelctl app service source git notes web https://example.com/app.git --target runtime --build-arg ORIGIN=https://notes.example.com --yes
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
piquelctl app route add notes notes.example.com web 3000 --visibility public --yes
piquelctl app route add notes admin.notes.example.com admin 8080 --yes
piquelctl app route redirect notes www.notes.example.com https://notes.example.com --visibility public --yes
piquelctl app route visibility notes admin.notes.example.com private --yes
piquelctl app route list notes
piquelctl app route remove notes notes.example.com --yes
piquelctl env visibility notes staging private --yes
piquelctl app job set notes migrate web --timeout-seconds 600 --yes -- notes migrate
piquelctl app job move notes migrate 1 --yes
piquelctl app job remove notes migrate --yes
piquelctl app variable set notes domain piquel.fr --yes
piquelctl app variable set notes domain staging.piquel.fr --env staging --yes
piquelctl app variable set notes web_replicas 3 --env production --yes
piquelctl app variable unset notes web_replicas --env production --yes
piquelctl app service replicas notes web '${{ vars.web_replicas }}' --yes
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
Route edits preserve other routes and use the inspected generation to reject
concurrent changes. `route redirect` defaults to status 308 and appends the
request path and query to the destination; use `--status` and
`--no-preserve-path` to change that. Routes are private (tailnet only) unless
added with `--visibility public`; `route visibility` changes an existing route,
and `env visibility` caps every route of one environment at `private` (or lifts
the cap with `public`) in the saved manifest. Deploy after saving to activate or
remove routing. `route list` shows each deployed route with its environment,
effective visibility, state, destination and the DNS records its hostname needs,
followed by the cause while it is not ready. `piquelctl status` also shows the
public and private listeners: the ingress mode (ports 80/443, or the Cloudflare
Tunnel's ID and edge connections), and the apps node's name, state and tailnet
addresses.
`job set` adds a job after the existing ones, or replaces the job with that name
in place, keeping its timeout unless `--timeout-seconds` is given (300 for a new
job). `job move` sets a job's 1-based position in the run order. Jobs run in
their saved order before each rollout; like routes, job edits
preserve the other jobs and use the inspected generation.
Each job inherits its referenced service's `depends_on`: those services and
their transitive dependencies start or update and pass health checks before
the job runs. Configure dependencies with `app service depends-on`. Other
services wait for all jobs to succeed; a failed job does not roll back its
dependencies. Jobs of dependency services must come earlier in the saved order.

`app variable set` sets a variable's default, or with `--env NAME` its value in
the environment named `NAME`, whether or not that environment exists yet. An
environment's value overrides the default, and a variable needs no default.
`true`, `false` and integers keep their type and anything else is text; `--string`
keeps text such as `3` as text. `app variable unset` removes the default or the
environment's value. Like routes, variable edits preserve the other variables and
use the inspected generation. Settings that accept variables, such as replicas,
limits, health check settings, environment values, commands and images, take a
`${{ vars.<name> }}` reference in place of a literal; quote it for the shell.
`app show` lists the defaults and per-environment values.

`app service rollout` replaces the service's rollout block: an omitted `--order`
derives the order from the mounts, and an omitted `--monitor-seconds` uses 30
seconds, so running it without flags restores the defaults. `app plan` lists each
service's effective order and monitor window, and warns when `start-first` is set
on a service with a writable volume.

`app plan --env NAME` renders the manifest for that environment: its changes
show rendered values, and it lists the value of every variable in scope, such as
`vars.domain` and `env.name`. A reference without a value fails the plan with
`variable_value_missing`. Without `--env`, the application's only environment is
used; with several, the plan compares the saved manifest as written. Plans warn
with `environment_block_unknown` about `[spec.environments.<name>]` blocks that
name no environment.

Command, argument, job command, and `depends-on` arrays preserve individual elements; place
command options before `--`. An empty array clears the setting. Optional limits and pinned
commits use `--clear`; health checks use `health clear`. Nested Git/health settings
require the corresponding source/check to be configured first. Use `--help` on
any command for its values and options.

Git-backed applications keep services, volumes, and the application name under
repository ownership. The repository URL and manifest path remain editable;
connecting points every environment at `--branch` (default `main`), and each
environment's branch then changes with `env branch`. Disconnect explicitly to
retain the saved manifest (the one last fetched by any environment) and edit it
locally:

```console
piquelctl app repository connect notes https://example.com/infra.git infra/app.toml --yes
piquelctl app repository path notes corrected/app.toml --yes
piquelctl env branch notes production release --yes
piquelctl app repository disconnect notes --yes
```

## Running commands

`app exec` runs a one-off command in a running task of a deployed service, for
administrative work such as creating an invitation or opening a console:

```console
piquelctl app exec piquel-fr auth -- auth-service invite create
piquelctl app exec piquel-fr db -i -- psql < dump.sql
piquelctl app exec piquel-fr auth -it -- /bin/sh
```

It runs in the application's only environment; with several, `--env ENV` names
one. The command runs inside the task, so it shares the task's image, environment,
secrets, mounts and networks. It works over every transport, uses account
authentication and needs no SSH access; the account (or token) needs
[`apps:exec`](authorization.md) on the application. Output streams to stdout and stderr, and
`piquelctl` exits with the command's exit code. `-i` forwards standard input.
`-t` allocates a terminal, implies `-i`, requires a terminal on standard input
and puts it in raw mode, so Ctrl-C reaches the command. `--timeout` bounds the
WebSocket handshake, not the session.

The service needs a running task; otherwise the command fails with
`service_not_running`. The environment's history records `command_started` and
`command_finished` events with the account, task and exit code, never the
command. Ending `piquelctl` early closes the stream, but the command may keep
running in the task.

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
stable application ID: application-wide events such as edits and renames, and the
events of all its environments, including deleted ones. `--environment ID` filters
by stable environment ID. Deleting an application removes its history. Use `--cursor CURSOR` for subsequent
pages and `--limit N` (1–100, default 50). JSON includes the next cursor.

By default, apply with `--deploy`, delete, reconcile, deploy, and operation poll every 250 ms
until a terminal state. Supersession returns immediately with exit code 0 and
`outcome: "superseded"` in mutation command results (`state: "superseded"` on an
operation record). It does not wait for the replacement to deploy. An observed
failed attempt returns failure even if the controller will retry automatically.
`--no-wait` returns immediately. For long image pulls,
use a longer `--timeout` or return immediately and inspect the operation later.
Pressing Ctrl-C ends the local command, including confirmation and network
requests. It does not cancel an accepted server-side operation, which can still
be inspected with `piquelctl operation <id>`.

The commonly useful exit codes are 0 for success or supersession, 1 for a general error, 2 for
usage or input errors, 3 for conflicts, 4 for unavailable or timed
out requests, 5 for a failed operation, and 130 when local operation waiting is
interrupted.

The dashboard provides application management forms and recent application logs. Remote
registry management and advanced interactive CLI flows remain future work.

Use `piquelctl setup-link` on the daemon host to print the first-account setup
link (`--open` also opens it in the default browser); only the Unix socket serves
it. Use `piquelctl login` for passkey login through the browser, `whoami` to inspect
the current account and its grants, and `logout` to revoke it. On the daemon host,
root and the daemon's own user need no login: without a token, they act over the
Unix socket as the [host operator](authentication.md#the-host-operator), with
`admin` on every application, and `whoami` prints `host operator (uid 0)`. Run
that way, `piquelctl sign-in-link` prints a one-time link (valid for 10
minutes) that signs a browser in as the host operator for 12 hours; it raises a
security notification. `piquelctl account`
lists accounts (and live host operator browser sessions), replaces their grants
(`access`), and creates invitation (`invite`)
and passkey enrollment (`enroll`) links. `piquelctl token create|list|revoke`
manages API tokens for your account (`--tailnet` binds one to a tailnet user
or tag), and `login` accepts the same grant options
to ask for a limited session; see [authorization](authorization.md).
`piquelctl audit` reads the [audit trail](observability.md#audit-trail), one line
per request with the application or environment it addressed, filtered
by `--user` (a username, or any account ID, including a deleted account's),
`--credential`, and `--outcome`; `piquelctl audit verify` checks its
[hash chain](observability.md#tamper-evidence). When no administrator can sign
in, `sudo piquelctl recover-admin` on the daemon host prints a one-time link
that creates a new administrator; see
[recovering administrator access](authentication.md#recovering-administrator-access). Saved credentials are separate
from profiles. `PIQUELD_TOKEN` supplies an automation token; `--account` selects
a saved account. See [authentication](authentication.md) for details.

`app deploy` and `env deploy` fetch a repository-backed environment's manifest
from its branch, then explicitly resolve image or Git build sources. They
supersede pending work for the selected environment. Use `--yes` to skip
interactive confirmation, `--no-wait` to return after acceptance, or a longer
global `--timeout` for builds. `--branch NAME` or `--commit SHA` fetches a
repository-backed manifest from another revision for this deployment only,
without changing the environment's branch; for example, to test a feature
branch. Once the deployment finishes, they warn about anything it reported, such
as `manifest_connection_ignored` when the fetched file's `spec.manifest` names
another repository URL or path. The
server continues deployment if the CLI wait times out. `app reconcile` and
`env reconcile` retry or repair the latest
deployment snapshot and prepared target without refreshing sources.

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
(or `env logs APP [ENV]` with the same options) reads a recent Docker snapshot with timestamps, service, task, and stream labels.
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

`piquelctl builds list [--application ID] [--environment ID] [--cursor CURSOR]` lists one
page of build attempts and job runs, optionally of one application's environments or
of one environment. `piquelctl builds logs ID [--before BYTE_OFFSET]` reads the newest bounded
output page, or an older page before the supplied cursor. It prints the cursor
for loading older output when available. Both support `--json`.

Manually set secrets live in the application's secret store and are
write-only. Each lists the environments that may mount it, by default every
environment, including ones created later, and no previews:

```sh
piquelctl app secret notes list
piquelctl app secret notes set database-password --file ./password --yes
printf '%s' 'new-value' | piquelctl app secret notes set stripe-live --stdin --environments production --yes
piquelctl app secret notes access stripe-live --environments production,staging --previews
piquelctl app secret notes access stripe-live --all-environments --no-previews
piquelctl app secret notes delete database-password --yes
```

`list` shows each secret's version and who may mount it, naming environments.
`--environments a,b` (names or IDs; `--environments ''` for none) or
`--all-environments` sets the environments, and `--previews` or `--no-previews` the previews flag, which is
kept for when previews exist. Unset flags keep the current access. Environments
are stored by ID, so renaming one keeps its access and deleting one removes it.
Deploying, or `app plan --env`, for an environment that mounts a secret it may
not use fails with `secret_access_denied`.

Prefer protected files or a secure stdin producer over literal shell values in
real use. `--expected-generation` pins a write to inspected metadata; otherwise
the CLI reads the current generation before confirming. `--json` returns only
metadata. Replacement creates a new version for a later Deploy and does not
change running deployments. Deletion refuses secrets that any environment's
saved configuration or runnable deployment still uses.

Manifests can declare secrets that piqueld generates for each environment when
a deployment first mounts them instead; see
[application manifests](application-manifest.md). They are listed, regenerated
and deleted per environment. `regenerate` creates a new version for the next
deployment, to rotate a value or replace one discarded by key recovery; running
deployments keep theirs. `delete` removes an unused one:

```sh
piquelctl env secret notes --env staging list
piquelctl env secret notes --env staging regenerate database-password --yes
piquelctl env secret notes --env staging delete database-password --yes
```

`env show` lists each secret the environment mounts and where its value comes
from: generated for the environment, the application store (with its version),
missing, not allowed by the secret's access list, or discarded by key recovery,
which would fail the next deploy until replaced.

If the daemon's `secrets.key` is lost and no backup exists, recover by discarding
stored and generated values across ALL applications and environments:

```sh
piquelctl secrets recover-key --yes
```

The command requires confirmation (`--yes` for automation), refuses while the
current key still works, and reports affected environment, secret and version
counts. Running Docker services are left alone. `app secret APP list` and
`env secret APP list` mark discarded values as unavailable; set replacement
stored values using the same names, then Deploy explicitly, which also generates
new values for discarded generated secrets.
