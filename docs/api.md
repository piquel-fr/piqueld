# HTTP API

The versioned API is rooted at `/api/v1`. JSON responses use a `data` envelope;
list responses contain `items` and an opaque `next_cursor`. Errors use one
stable envelope with a machine-readable code, safe message, optional details,
and a request ID. Internal database, parser, and Docker sources are logged for
diagnostics but are not returned or persisted as raw messages.

The daemon serves the API over loopback TCP and a Unix-domain socket. The
browser dashboard is served only by the loopback TCP listener; the Unix socket
remains API-only. The typed client supports both native transports and a
same-origin browser transport. Durable state is always available through
bounded polling, while operation and application-log streams provide a
resumable live view for clients that support Server-Sent Events.

Supported resources and actions are:

- `GET /health` (unversioned liveness response)
- `GET /api/v1/system/status`
- `GET /api/v1/system/health` (authenticated liveness response)
- `GET /api/v1/system/readiness` (authenticated dependency readiness)
- `GET /api/v1/system/metrics` (optional authenticated Prometheus text)
- `GET /api/v1/openapi.json`
- `GET /api/v1/applications` and `GET /api/v1/applications/{id}`
- `GET /api/v1/applications/{id}/detail` (desired state, runtime summary, latest operation, and bounded diagnostics)
- `POST /api/v1/applications` and `PUT /api/v1/applications/{id}`
- `DELETE /api/v1/applications/{id}`
- `POST /api/v1/applications/plan` and `POST /api/v1/applications/{id}/plan`
- `POST /api/v1/applications/{id}/reconcile`
- `GET /api/v1/applications/{id}/status`
- `GET /api/v1/applications/{id}/logs` and `/events`
- `GET /api/v1/operations/{id}`
- `GET /api/v1/operations/{id}/events` and `/builds`
- `GET /api/v1/builds/{id}` and `/logs`
- `GET /api/v1/secrets`, `POST/PUT/DELETE /api/v1/secrets/{name}`
- `GET /api/v1/state/export`, `POST /api/v1/state/import/confirm`, and
  `POST /api/v1/state/import`

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

`POST /api/v1/applications/{id}/delete-plan` accepts the same
generation-bound deletion request as `DELETE`, observes runtime state, and
returns the exact server-generated removal, wait, diagnostic, risk, and
retain-volume actions without changing desired state.

The public client contracts live in `piqueld-client`; persistence uses internal
store rows and converts them to these DTOs at the API boundary. The detail DTO
contains only sanitized, bounded runtime summaries and diagnostics, never raw
Docker labels, environment, or daemon-internal errors. Secret endpoints return
metadata and references only; plaintext values are accepted as bounded binary
requests and are never returned. State replacement is transactional and bound
to the exact archive digest through a single-use confirmation. The essential
CLI workflow is documented in [`docs/piquelctl.md`](piquelctl.md).

The TCP router gives exact API, health, and OpenAPI routes precedence over the
static dashboard. Unknown `/api`, `/health`, and `/openapi` paths remain JSON
errors; only non-reserved extensionless browser paths receive the dashboard
shell. The asset reader rejects traversal and symlink resolution, enforces a
16 MiB bound, supports HEAD and one byte range, caches only fingerprinted
assets, and applies CSP plus browser hardening headers. Missing extensionful
assets remain 404s. The Unix router has no static fallback.

## Transport security

The Unix socket uses filesystem ownership and mode `0660`. Loopback TCP is
fail-closed unless a protected bearer token is configured or an explicitly
trusted loopback proxy is enabled for a validated `Tailscale-User-Login`
identity. Credential files are opened without symlink traversal, must be
private regular files outside `/nix/store`, and are zeroized after loading.
Allowed browser origins are exact HTTP(S) origins; wildcard CORS is not
accepted. Header/body limits, request deadlines, a shared concurrency budget,
and sanitized errors apply to both listeners. Authentication and proxy identity
headers are removed before tracing and handlers run.
