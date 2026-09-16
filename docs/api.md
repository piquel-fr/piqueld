# HTTP API

The API is rooted at `/api/v1` over a Unix socket and optional localhost or Tailscale TCP.
Responses use a `data` envelope; lists contain `items` and an opaque
`next_cursor`. Errors expose a safe message, code, details, and request ID.
Clients poll for progress.

Application list items contain only `id`, `name`, generation metadata, deletion
intent, and timestamps. Read `/api/v1/applications/{id}` when the complete
normalized manifest is needed.

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/api/v1/system/status` | Daemon status |
| GET | `/api/v1/system/configuration` | Effective read-only host settings |
| GET | `/api/v1/openapi.json` | Generated API schema |
| GET | `/api/v1/applications` | Paginated application summaries (up to 100 per page) |
| GET | `/api/v1/applications/{id}` | Full latest accepted application intent |
| GET | `/api/v1/applications/{id}/detail` | Intent, resolved generation, observed runtime, operation, diagnostics |
| GET | `/api/v1/applications/{id}/status` | Intent progress and separate runtime health |
| POST | `/api/v1/applications/plan` | Preview a manifest without pulling images |
| POST | `/api/v1/applications/apply` | Save configuration by name; `?deploy=true` also deploys |
| POST | `/api/v1/applications/{id}/deploy` | Deploy the inspected saved revision with fresh source resolution; supersede pending work |
| GET | `/api/v1/applications/{id}/deployments` | Deployment snapshots, newest first, three per page |
| GET | `/api/v1/applications/{id}/deployments/{deployment}/attempts` | Retained outcomes, newest first, 100 per page |
| DELETE | `/api/v1/applications/{id}` | Request deletion; no body |
| POST | `/api/v1/applications/{id}/reconcile` | Repair latest intent without refreshing prepared digests |
| POST | `/api/v1/applications/{id}/rename` | Rename an idle application without redeployment |
| GET | `/api/v1/operations/{id}` | Inspect progress, attempt count, and safe diagnostics |
| GET | `/api/v1/events` | Paginated informational history, oldest first |

Saving and deployment acceptance work while Docker is unavailable. Execution
errors are recorded asynchronously. Preview requires runtime observation and
returns `503 docker_unavailable` during an outage. Application detail remains
readable and reports unavailable runtime observation as a diagnostic.

Apply and plan accept JSON `{ "manifest": ..., "expected_generation": 3,
"expected_application_id": "app-..." }`, or complete TOML with
`Content-Type: application/toml` or `text/toml`. TOML preconditions use
`X-Expected-Generation` and `X-Expected-Application-Id`.

Apply, deploy, delete, and rename require preconditions unless the endpoint is explicitly
called with the query parameter `force=true`. Missing preconditions return 400
`precondition_required`. Apply requires generation zero to create an absent name,
or both the inspected application ID and generation to update an existing name.
Deploy and delete require `expected_generation` in its query; rename takes it in JSON.
Revision mismatches return 409 `generation_conflict`; identity mismatches return
409 `identity_conflict`. Checks and acceptance are atomic.

Force overrides both revision and name-based identity preconditions, even if stale
values were supplied. Forced apply overwrites whichever application currently has
the manifest name, or creates one if absent. ID-based mutations still target their
URL's ID. Force never bypasses manifest validation, resource ownership, or rename
busy/name-collision checks. There is no authorization logic for force yet.

Reconcile acts on the current intent without requiring preconditions;
it accepts an optional `expected_generation` query for callers that want one.
Reconcile can continue an already-requested deletion. Preview preconditions are
also optional. The CLI supplies apply/delete/rename preconditions automatically;
`--yes` skips confirmation and `--force` requests the override independently.

Configuration generation starts at 1 and advances on saves, changed names, and
deletion intent. Apply replaces the full configuration without merging. It returns
200 with `SavedApplication` (`application_id`, `generation`, and null `operation_id`).
With `?deploy=true`, apply atomically saves and deploys, returning 202 with a populated
`operation_id`. Saving during deletion is rejected.

Deploy captures exactly the inspected saved revision, returning 202 with
`AcceptedOperation`. Every explicit Deploy creates a new snapshot and supersedes
pending work, even for unchanged configuration. It resolves image tags again;
matching healthy containers need no restart. Empty applications are valid: an
empty deployment removes services and networks while retaining volume data.

Reconciliation and retries use deployment snapshots and their prepared digests,
never newer saved edits. Configuration saves do not supersede operations. Deployments and
attempt outcomes remain indefinitely until application deletion. The deployments
response distinguishes `current_target`, `last_successful`, and mutable operation
progress; last successful does not imply automatic rollback after a failed rollout.
History endpoints accept `cursor` for subsequent pages.

Mutation endpoints accept an optional `Idempotency-Key` (1–128 ASCII letters,
digits, `-`, `_`, `.`, or `:`). The CLI generates one UUID per command and reuses
it for transport retries. The daemon atomically stores the accepted response and a
fingerprint of the normalized request in SQLite for 24 hours. A matching retry
returns the original response before checking the current generation, even after
restart, failure, or supersession. It never restarts the operation. Different input
under the same key returns 409 `request_id_conflict`. Expired keys are treated as
new requests subject to current preconditions. Receipts contain no manifests or
raw request bodies, and do not guarantee exactly-once Docker effects. Force is
part of request identity: retrying a forced request replays its receipt instead of
overwriting intervening changes again. A new forced command needs a new key.

Rename accepts JSON `{ "name": "new-name", "expected_generation": 3 }` and returns
200 with `RenamedApplication`. It rejects pending/running operations and deletion
intent with 409 `application_busy`, and occupied names with 409
`application_name_collision`. It preserves the stable ID, runtime resources, and
operation identity; a changed name advances generation and records an event.
Update the manifest's name before subsequent name-based apply.

Operations have kind `apply`, `refresh` (the stored kind used by Deploy), or `delete`
and state `requested`,
`running`, `succeeded`, `failed`, `cancelled`, or `superseded`. New intent marks
pending/running older operations `superseded`, separately from cancellation.
The CLI treats supersession as success with an explicit outcome and stops waiting
immediately, without following the replacement. Each execution increments
`attempt`. Deployment attempt outcomes remain available even after event pruning.
Deletion retains volumes and completes only after runtime absence is verified.
It then removes the application, operations, deployments, attempts, events, and
receipts. Clients waiting for deletion poll application absence; its operation
endpoint also returns 404 after cleanup.

Preview returns 200 with a `PlanView`, no durable changes, and no image pulls.
The response includes the inspected generation (zero for an absent name), an
`identical` flag, latest operation, redacted manifest field changes, and a runtime
plan. Manifest differences compare against the last deployment snapshot, not saved
configuration. Image tags report resolution requirements even when unchanged,
matching Deploy's refresh behavior. Environment,
command, argument, and health-check values are redacted in previews, including
runtime actions. Execution computes its own unredacted plan after preparation.
Previews cannot freeze mutable tags or runtime state.

Events accept optional `application_id`, `cursor`, and `limit` (1–100, default 50).
They survive ordinary operation pruning but are removed with their application.
Event retention is independently configured by `retention.event_days` (default
30; zero disables pruning). Events contain safe diagnostics and identifiers,
never manifests, environment values, or raw Docker errors. Failure events preserve
`error_code`, `phase`, and `resource`; operation reads expose the current phase and
resource. Significant resource mutations and active-target repairs are recorded,
while unchanged observations and timer ticks are omitted.

The unauthenticated TCP API trusts every caller able to reach its configured
localhost or Tailscale listeners. The dashboard
is served at `/dashboard/`; `/health` is an unversioned TCP liveness endpoint.
The Unix socket serves the API alone. See [the CLI guide](piquelctl.md) and
[the generated contract](openapi-v1.json).

`POST /api/v1/applications/{id}/deploy` prepares all sources again, including
Git builds. It requires the inspected generation unless forced and supersedes
pending work. An identical idempotency-key replay returns the original acceptance.
Use `piquelctl deploy NAME --yes` to request and wait for deployment.

When `spec.manifest` is configured, Deploy first fetches the selected manifest.
Its `refresh` operation records `fetching_manifest` progress and a `manifest_fetched`
event with the commit hash. Generation changes only when a changed candidate
passes preparation; initial acceptance returns the currently stored generation.
Failures use `manifest_not_found`, `manifest_fetch_failed`, or `manifest_invalid`.
Direct apply may repair manifest connection settings but rejects changes to
repository-managed runtime fields with `409 repository_managed`.
The legacy refresh endpoint resolves stored service sources without fetching a
new manifest; reconcile retries the latest operation with its saved inputs.

`GET /api/v1/applications/{id}/manifest` downloads saved configuration as `application/toml`, with an attachment filename and `Cache-Control: no-store`. It does not observe Docker or resolve sources.

`GET /api/v1/applications/{id}/logs` reads Docker container output for services
owned by this application and daemon instance. Optional `service` filters by
logical service name; `tail` defaults to 200 (1–1000) and `since_seconds` to 3600
(1–86400). Records include timestamp, service, task ID, stream and message.
Snapshots are capped at 1 MiB of collected text and 256 tasks, with `truncated`
indicating a partial result. Docker retains the source logs; removed containers
have no available history. No output is stored by piqueld.

`GET /api/v1/system/readiness` reports top-level `ready` plus separate `database`,
`docker`, and `swarm` verdicts. Each verdict has `status: "ready"` or
`status: "failed"`; failed verdicts include a required safe `message`, while ready
verdicts omit it. HTTP 200 means deployment dependencies are available; HTTP 503
carries the same structured envelope when they are not. Probes have bounded
deadlines and do not initialize Swarm or repair resources. Docker being unavailable
does not block configuration saves or history reads. `/health` remains a
process-liveness endpoint. No metrics or external registry/ingress checks are
introduced.

`GET /api/v1/builds` lists attempts newest first, with optional `application_id`,
`cursor`, and `limit` (1–100, default 50). Each executed Git-service preparation
creates an independent record before checkout. Image pulls do not create records.
Outcomes are running, succeeded, failed, or interrupted. Resolved commits and
image IDs are recorded when available; retries create new attempts.

`GET /api/v1/builds/{id}/logs?offset=0` reads at most 64 KiB of output. Follow
`next_offset` for more. Output is decoded as lossy UTF-8; offsets count original
bytes. Truncation and expiration are explicit. Output retains the configured
prefix, defaults to 4 MiB per attempt and expires 30 days after completion.
Metadata survives operation pruning and is deleted with its application.

Application secret endpoints expose metadata only:

- `GET /api/v1/applications/{id}/secrets` lists names, current generations and update times.
  Its unpaginated metadata array is returned directly in `data`, without `items` or `next_cursor`.
- `PUT /api/v1/applications/{id}/secrets/{name}` accepts an `application/octet-stream`
  value of 1–512000 bytes. `X-Expected-Generation: 0` creates; a current generation
  replaces. Each application supports at most 100 logical secrets.
- `DELETE` at the same path requires `X-Expected-Generation` and refuses references
  in saved configuration, the current runnable deployment, or the active target.
  It removes Docker versions before removing encrypted records. A version conflict
  returns 409; missing or invalid key material returns 503 for value-dependent work.

Values never appear in responses, manifests or deployment snapshots. Deployments
pin immutable versions during effective-input preparation; retries preserve those
pins. Earlier ciphertext versions remain until logical-secret or application deletion.
