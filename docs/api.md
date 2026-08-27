# HTTP API

The versioned API is rooted at `/api/v1`. JSON responses use a `data` envelope;
list responses contain `items` and an opaque `next_cursor`. Errors use one
stable envelope with a machine-readable code, safe message, optional details,
and a request ID. Internal database, parser, and Docker sources are logged for
diagnostics but are not returned or persisted as raw messages; malformed
request bodies additionally log the safe field path that was rejected.

The daemon serves the API over loopback TCP and a Unix-domain socket. The
read-only browser dashboard is served only by the loopback TCP listener; the
Unix socket remains API-only. The typed client supports both native transports
and a same-origin browser transport. The API is intentionally polling based:
clients fetch application status and operation state instead of opening event
streams.

Because the API is unauthenticated, TCP requests whose `Host` is not loopback
(`localhost`, `127.0.0.1`, `[::1]`) are rejected with 403 `host_not_allowed`;
this blocks DNS-rebinding browsers. Unix-socket requests carry no host and are
always accepted. Unsupported methods on known routes answer 405 with an `Allow`
header.

Supported resources and actions are:

- `GET /health` (unversioned liveness response outside the OpenAPI contract)
- `GET /api/v1/system/status`
- `GET /api/v1/openapi.json`
- `GET /api/v1/applications` and `GET /api/v1/applications/{id}`
- `GET /api/v1/applications/{id}/detail` (desired state, runtime summary, latest operation, and bounded diagnostics)
- `POST /api/v1/applications` and `PUT /api/v1/applications/{id}`
- `DELETE /api/v1/applications/{id}`
- `POST /api/v1/applications/plan` and `POST /api/v1/applications/{id}/plan`
- `POST /api/v1/applications/{id}/reconcile`
- `GET /api/v1/applications/{id}/status`
- `GET /api/v1/operations/{id}`

Creation requires an `Idempotency-Key`; replacement and deletion accept one as
well. Requests carrying more than one `Idempotency-Key` header value are
rejected. Only the key's SHA-256 digest and the normalized request hash are
stored; repeating a matching mutation replays the original operation identity,
a cancelled or failed bound delete/replace is resurrected instead of replayed
dead, and reusing a key for a different canonical spec conflicts with 409.
Bindings live in the operation journal and are deleted together with their
operation by the store's retention pruning, so key reuse after that horizon
starts a new operation instead of replaying. All mutations use optimistic generation checks (JSON bodies carry
`expected_generation`; deletion requires one) and return a durable operation
identity with HTTP 202. Polling `GET /operations/{id}` exposes safe operation
and step diagnostics.

Deletion carries its `expected_generation` as a JSON request body on `DELETE`.
Some HTTP clients and proxies strip bodies from DELETE requests; such clients
should use the Unix-socket transport, the typed client, or a replacement
carrying an explicit empty spec instead of relying on intermediaries to
forward the body.

Create, replace, and plan accept structured JSON or a complete TOML manifest with
`Content-Type: application/toml` (also `text/toml`). TOML replacement carries
`X-Expected-Generation`. Plan endpoints perform no durable mutation and report
image resolution requirements when the runtime cannot yet produce a concrete
desired plan. Preview endpoints always answer 200 with a `PlanView`; callers
inspect its blocking diagnostics before deciding whether to mutate.

The public client contracts live in `piqueld-client`; persistence uses internal
store rows and converts them to these DTOs at the API boundary. The detail DTO
contains only sanitized, bounded runtime summaries and diagnostics, never raw
Docker labels, environment, or daemon-internal errors. The essential CLI
workflow is documented in [`docs/piquelctl.md`](piquelctl.md). Authentication,
mutating browser controls, and additional transports are outside this product
slice.

The TCP router composes the API, `/health`, and the optional dashboard. When UI
assets are enabled, `/` and `/dashboard` permanently redirect to
`/dashboard/`; the dashboard serves its static files below that prefix and
falls back to its shell for extensionless Leptos routes. Unknown `/api/...`
paths remain structured JSON errors, while paths outside `/api` and
`/dashboard` remain ordinary 404s. The Unix router uses the API routes directly
and has no health, dashboard, or static fallback.
