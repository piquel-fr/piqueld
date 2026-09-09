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
| GET | `/api/v1/operations/{id}` | Inspect progress, attempt count, and safe diagnostics |
| GET | `/api/v1/events` | Paginated informational history, oldest first |

Apply and plan accept JSON `{ "manifest": ..., "expected_generation": 3 }`,
or complete TOML with `Content-Type: application/toml` or `text/toml`. The optional
TOML precondition is `X-Expected-Generation`. Delete, reconcile, and refresh accept
an optional `expected_generation` query parameter. A mismatch returns 409
`generation_conflict`. Zero requires an absent name on apply. Without a supplied
generation, a mutation targets current intent unconditionally.

Generation starts at 1 and advances for a changed normalized manifest or deletion
intent. Comments and ordering do not cause changes. Full apply replaces the
manifest without merging. Refresh, reconciliation, attempts, and runtime health
never advance generation. Applying the same manifest while deletion is intended
reverses deletion and advances generation.

Apply returns 202 and an `AcceptedOperation` before image resolution. Preparation
failures are reported on the operation. An identical manifest returns the current
operation without resolution or scheduling; if it failed, it is requested again.
Refresh explicitly resolves the current manifest: active refreshes are reused,
failed refreshes retry, and a refresh after success starts a new operation.
Refresh is rejected during deletion. Reconcile reuses stored digests for latest
intent, retrying preparation only when it did not complete.

There are no idempotency keys or exact request replay guarantees. Clients may
retry a transport failure. In particular, a refresh retry after completion can
start another refresh; a conditioned mutation retry after a lost success response
can conflict because the first request advanced generation.

Operations have kind `apply`, `refresh`, or `delete` and state `requested`,
`running`, `succeeded`, `failed`, or `cancelled`. Each execution increments
`attempt`. Previous outcomes remain in events even when the operation is reused.
Deletion retains volumes and completes only after runtime absence is verified.

Preview returns 200 with a `PlanView`, no durable changes, and no image pulls.
An unchanged apply has an empty plan. Changed manifests identify image-resolution
requirements; execution checks the concrete plan after resolving images. Preview
cannot freeze mutable tags or runtime state.

Events accept optional `application_id`, `cursor`, and `limit` (1–100, default 50).
They include history for deleted applications and survive operation pruning.
Event retention is independently configured by `retention.event_days` (default
30; zero disables pruning). Events contain safe diagnostics and identifiers,
never manifests, environment values, or raw Docker errors.

The unauthenticated TCP API accepts only loopback hosts. The read-only dashboard
is served at `/dashboard/`; `/health` is an unversioned TCP liveness endpoint.
The Unix socket serves the API alone. See [the CLI guide](piquelctl.md) and
[the generated contract](openapi-v1.json).
