# HTTP API

The API is rooted at `/api/v1` over a Unix socket and optional loopback TCP.
Responses use a `data` envelope; lists contain `items` and an opaque
`next_cursor`. Errors expose a safe message, code, details, and request ID.
Clients poll for progress.

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/api/v1/system/status` | Daemon status |
| GET | `/api/v1/openapi.json` | Generated API schema |
| GET | `/api/v1/applications` | Paginated applications |
| GET | `/api/v1/applications/{id}` | Latest accepted application intent |
| GET | `/api/v1/applications/{id}/detail` | Intent, resolved generation, observed runtime, operation, diagnostics |
| GET | `/api/v1/applications/{id}/status` | Intent progress and separate runtime health |
| POST | `/api/v1/applications/plan` | Preview a manifest without pulling images |
| POST | `/api/v1/applications/apply` | Accept a full manifest by name |
| DELETE | `/api/v1/applications/{id}` | Request deletion; no body |
| POST | `/api/v1/applications/{id}/reconcile` | Repair latest intent without refreshing prepared digests |
| POST | `/api/v1/applications/{id}/refresh` | Explicitly refresh image references |
| POST | `/api/v1/applications/{id}/rename` | Rename an idle application without redeployment |
| GET | `/api/v1/operations/{id}` | Inspect progress, attempt count, and safe diagnostics |
| GET | `/api/v1/events` | Paginated informational history, oldest first |

Apply and plan accept JSON `{ "manifest": ..., "expected_generation": 3 }`,
or complete TOML with `Content-Type: application/toml` or `text/toml`. The optional
TOML precondition is `X-Expected-Generation`. Delete, reconcile, and refresh accept
an optional `expected_generation` query parameter. A mismatch returns 409
`generation_conflict`. Zero requires an absent name on apply. Without a supplied
generation, a mutation targets current intent unconditionally. Apply and plan also
accept `expected_application_id` in JSON or `X-Expected-Application-Id` with TOML.
An identity mismatch returns 409 `identity_conflict`, protecting against a name
being deleted/recreated or renamed between preview and apply. The CLI sends these
preconditions automatically; `--force` skips confirmation only.

Generation starts at 1 and advances for a changed normalized manifest or deletion
intent. Comments and ordering do not cause changes. Full apply replaces the
manifest without merging. Refresh, reconciliation, attempts, and runtime health
never advance generation. Applying the same manifest while deletion is intended
reverses deletion and advances generation.

Apply returns 202 and an `AcceptedOperation` before image resolution. Preparation
failures are reported on the operation. An identical manifest returns the current
operation without resolution or scheduling, even after failure. Explicit reconcile
requests another attempt.
Refresh explicitly resolves the current manifest: active refreshes are reused,
failed refreshes retry, and a refresh after success starts a new operation.
Refresh is rejected during deletion. Reconcile reuses stored digests for latest
intent, retrying preparation only when it did not complete.

Mutation endpoints accept an optional `Idempotency-Key` (1–128 ASCII letters,
digits, `-`, `_`, `.`, or `:`). The CLI generates one UUID per command and reuses
it for transport retries. The daemon atomically stores the accepted response and a
fingerprint of the normalized request in SQLite for 24 hours. A matching retry
returns the original response before checking the current generation, even after
restart, failure, or supersession. It never restarts the operation. Different input
under the same key returns 409 `request_id_conflict`. Expired keys are treated as
new requests subject to current preconditions. Receipts contain no manifests or
raw request bodies, and do not guarantee exactly-once Docker effects.

Rename accepts JSON `{ "name": "new-name", "expected_generation": 3 }` and returns
200 with `RenamedApplication`. It rejects pending/running operations and deletion
intent with 409 `application_busy`, and occupied names with 409
`application_name_collision`. It preserves the stable ID, runtime resources, and
operation identity; a changed name advances generation and records an event.
Update the manifest's name before subsequent name-based apply.

Operations have kind `apply`, `refresh`, or `delete` and state `requested`,
`running`, `succeeded`, `failed`, or `cancelled`. Each execution increments
`attempt`. Previous outcomes remain in events even when the operation is reused.
Deletion retains volumes and completes only after runtime absence is verified.

Preview returns 200 with a `PlanView`, no durable changes, and no image pulls.
The response includes the inspected generation (zero for an absent name), an
`identical` flag, latest operation, redacted manifest field changes, and a runtime
plan. Identical intent has an empty plan; this does not assert runtime health.
Unchanged service image references reuse active digests for both preview and apply;
new/changed references report resolution requirements. Runtime unavailability is
an informational diagnostic, so manifest changes remain available. Environment,
command, argument, and health-check values are redacted in previews, including
runtime actions. Execution computes its own unredacted plan after preparation.
Previews cannot freeze mutable tags or runtime state.

Events accept optional `application_id`, `cursor`, and `limit` (1–100, default 50).
They include history for deleted applications and survive operation pruning.
Event retention is independently configured by `retention.event_days` (default
30; zero disables pruning). Events contain safe diagnostics and identifiers,
never manifests, environment values, or raw Docker errors. Failure events preserve
`error_code`, `phase`, and `resource`; operation reads expose the current phase and
resource. Significant resource mutations and active-target repairs are recorded,
while unchanged observations and timer ticks are omitted.

The unauthenticated TCP API accepts only loopback hosts. The read-only dashboard
is served at `/dashboard/`; `/health` is an unversioned TCP liveness endpoint.
The Unix socket serves the API alone. See [the CLI guide](piquelctl.md) and
[the generated contract](openapi-v1.json).
