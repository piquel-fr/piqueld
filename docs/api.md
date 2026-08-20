# HTTP API

The versioned API is rooted at `/api/v1`. JSON responses use a `data` envelope;
list responses contain `items` and an opaque `next_cursor`. Errors use one
stable envelope with a machine-readable code, safe message, optional details,
and a request ID. Internal database, parser, and Docker sources are logged for
diagnostics but are not returned or persisted as raw messages.

The daemon serves the API over loopback TCP and a Unix-domain socket. The
read-only browser dashboard is served only by the loopback TCP listener; the
Unix socket remains API-only. The typed client supports both native transports
and a same-origin browser transport. The API is intentionally polling based:
clients fetch application status and operation state instead of opening event
streams.

Supported resources and actions are:

- `GET /health` (unversioned liveness response)
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
well. Only the key's SHA-256 digest and the normalized request hash are stored;
repeating a matching mutation replays the original operation identity. All
mutations use optimistic generation checks and return a durable operation
identity with HTTP 202. Polling `GET /operations/{id}` exposes safe operation
and step diagnostics.

Create, replace, and plan accept structured JSON or a complete TOML manifest with
`Content-Type: application/toml` (also `text/toml`). TOML replacement carries
`X-Expected-Generation`. Plan endpoints perform no durable mutation and report
image resolution requirements when the runtime cannot yet produce a concrete
desired plan.

The public client contracts live in `piqueld-client`; persistence uses internal
store rows and converts them to these DTOs at the API boundary. The detail DTO
contains only sanitized, bounded runtime summaries and diagnostics, never raw
Docker labels, environment, or daemon-internal errors. The essential CLI
workflow is documented in [`docs/piquelctl.md`](piquelctl.md). Authentication,
mutating browser controls, and additional transports are outside this product
slice.

The TCP router gives exact API, health, and OpenAPI routes precedence over the
static dashboard. Unknown `/api`, `/health`, and `/openapi` paths remain JSON
errors; only non-reserved extensionless browser paths receive the dashboard
shell. Missing extensionful assets remain 404s. The Unix router has no static
fallback.
