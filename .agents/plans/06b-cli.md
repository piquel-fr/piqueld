# Plan 06B — Add the essential `piquelctl` workflow

## Goal

Make the Plan 06 product usable from a terminal through a small, safe CLI. Cover the
complete everyday prebuilt-image workflow without importing advanced features from
the old Plan 11.

## Commands and global options

Implement these commands:

```text
piquelctl status
piquelctl list
piquelctl show <name-or-id>
piquelctl plan --file <application.toml>
piquelctl apply --file <application.toml>
piquelctl delete <name-or-id>
piquelctl operation <operation-id>
```

Support only options needed by this workflow:

- `--socket <path>` for Unix-socket transport, which is the default;
- `--url <loopback-url>` for an explicit TCP endpoint;
- `--timeout <duration>` for bounded requests/waits;
- `--json` for machine-readable output;
- `--yes` to confirm an apply or delete noninteractively;
- `--no-wait` to print an accepted operation and return; and
- `--expected-generation <n>` when an operator deliberately supplies concurrency
  state.

Use `clap` for parsing. Do not add a CLI framework or configuration/profile system.

## Deliverables

- A `piquelctl` binary that depends on the public client, never daemon persistence or
  Docker implementation modules.
- Human-readable output for interactive use and clean public-DTO JSON for scripts.
- Safe plan-before-apply behavior, optimistic concurrency, confirmation, operation
  polling, and useful failure diagnostics.
- Focused command tests across Unix and TCP transports.
- A short operator quickstart using the CLI.

## Work

### 1. Connection and input handling

1. Default to the daemon's documented Unix socket. Accept an explicit loopback URL;
   reject ambiguous simultaneous transport options.
2. Load TOML manifests from an explicit file with a sensible size bound and preserve
   parse/validation field context. Do not add stdin manifests in this increment.
3. Resolve `<name-or-id>` deterministically. Attempt a syntactically valid ID
   directly; otherwise page through applications by name using the public client.
   Report zero or multiple matches clearly.
4. Apply timeouts to requests and operation waits. Avoid an unbounded retry loop.

### 2. Read commands

1. `status` reports daemon availability/version and enough capability information to
   diagnose the connection.
2. `list` handles pagination and shows application identity, generation, desired
   replicas, and concise reconciliation status.
3. `show` presents desired state, observed/convergence state, and the latest useful
   diagnostic without dumping internal database records.
4. `operation` fetches an operation once or polls until terminal state unless
   `--no-wait` applies. Ctrl-C stops the local wait; it must not claim that the
   server-side operation was cancelled.

### 3. Plan and apply safely

1. `plan` chooses create or replace by manifest name. For replacement, obtain the
   current application generation unless the user supplied
   `--expected-generation`.
2. Render plan actions, reasons, risks, and retained/destructive effects. A blocked
   plan exits without mutation and explains the blocker.
3. `apply` always obtains and displays the plan first. Require a TTY confirmation
   unless `--yes` is present; fail closed in a noninteractive terminal without
   `--yes`.
4. Submit the same expected generation used for planning. Surface conflicts rather
   than automatically fetching and overwriting newer state.
5. Generate one idempotency key per mutating command invocation and reuse it for any
   safe transport retries during that invocation.
6. Poll the accepted operation to a terminal state by default. `--no-wait` prints the
   operation identifier and returns after acceptance.

### 4. Delete safely

1. Resolve and display the target before confirmation. State explicitly that
   managed services/network are removed and named volumes are retained.
2. Require confirmation using the same interactive/noninteractive rules as apply.
3. Use optimistic concurrency if the API supports it, then poll the deletion
   operation by default.

### 5. Output and errors

1. Human output is concise and optimized for decisions, not a serialization dump.
2. With `--json`, stdout contains only a documented public response/DTO shape.
   Diagnostics and progress belong on stderr.
3. Preserve daemon validation paths, conflict details, operation failures, and
   connection causes. Never replace them with a generic failure string.
4. Use a small set of conventional nonzero exits. Do not design the elaborate stable
   exit-code taxonomy reserved for the advanced CLI plan.

## Explicitly out of scope

- Profiles, configuration files, authentication/tokens, completion generation, and
  remote endpoint discovery.
- Secret, build, registry, route, log, import/export, or state-archive commands.
- SSE/event streaming, progress animations, interactive editors, or conflict merges.
- A promise that human output is stable; only `--json` is intended for scripts.
- Any direct dependency on daemon, database, or Docker crates.

## Verification

- Parser tests cover every command, option conflict, required confirmation, and
  invalid timeout/file input.
- In-process CLI tests cover status, paginated list, show by ID/name, create plan,
  replacement plan, apply success/failure/conflict, delete with retained-volume
  notice, operation polling, `--no-wait`, timeout, and interrupted waiting.
- Run representative tests over both Unix socket and loopback TCP.
- Snapshot a small set of human outputs and validate JSON structurally against
  public DTOs. Assert stdout remains clean in JSON mode.
- Verify idempotency-key reuse on retry and that repeated name lookup handles
  pagination.
- Run the repository's canonical `just` validation.

## Done when

Starting from a manifest file, an operator can inspect the daemon, plan and apply an
application, inspect convergence, and delete it safely without curl or database
access. Every mutation is planned, generation-aware, confirmable, retry-safe, and
observable to completion.

## Handoff

Document the command surface, default socket and override behavior, JSON shapes,
exit behavior, confirmation rules, polling interval/timeout, and any API/client
change required by the CLI. Identify advanced Plan 11 features that remain deferred.
