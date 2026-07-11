# Plan 11 — Complete `piquelctl` operator CLI

## Goal

Build the supported command-line workflow entirely on `piqueld-client`, with safe
secret input, human-readable defaults, stable JSON, and automation-friendly exits.

## Deliverables

- Commands from design section 18.4: status; application list/show/plan/apply/export/
  delete/logs; secret list/set/delete; operation watch; state export/import.
- Connection profiles for Unix socket and loopback/Tailscale HTTP, with token read
  from protected file/environment as defined by Plan 13 (never printed).
- TOML manifest loading plus expected-generation handling and clear conflict output.
- Human output with concise plan/action risk, operation progress, task/build logs,
  degraded diagnostics, and retained-volume notice.
- `--output human|json` (or equivalent), stable JSON schemas, quiet/noninteractive
  behavior, timeouts, and documented stable exit-code categories.
- Secret set reads stdin by default or a permission-checked file; no plaintext value
  command argument.

## Work

1. The canonical binary/name is `piquelctl` as specified in sections 16 and 18;
   treat the isolated `piqueldctl` spelling in the secret example as a typo.
2. Route every operation through the public client. Do not link daemon store/Docker
   modules into the CLI.
3. Make apply show/obtain the current generation and reject stale writes. Support a
   deliberate noninteractive mode without inventing automatic conflict resolution.
4. Stream SSE with reconnect/last-event handling and fall back to operation polling
   only when needed. Handle Ctrl-C by ending the watch, not falsely cancelling the
   server operation unless an explicit future API supports it.
5. Write binary state archives to explicit files/stdout without terminal corruption;
   require explicit replace confirmation on import and prevent accidental binary
   output to an interactive TTY.
6. Keep JSON stdout clean; diagnostics/progress go to stderr.

## Verification

- CLI integration tests run against an in-process/test daemon for every command and
  both Unix/TCP transports.
- Snapshot human/JSON output and stable exit codes for validation, auth, conflict,
  unavailable daemon, failed operation, and interrupted stream.
- Secret canary tests inspect argv, output, logs, and shell-visible errors.
- End-to-end fixture: plan/apply/watch/log/export/delete with retained volume.

## Done when

An operator can complete every non-UI acceptance workflow safely from the CLI, and
scripts have predictable JSON and exit behavior.

