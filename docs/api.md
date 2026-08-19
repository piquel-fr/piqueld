# HTTP API

The versioned API is rooted at `/api/v1`. JSON responses use a `data` envelope;
list responses contain `items` and an opaque `next_cursor`. Errors use one
stable envelope with a machine-readable code, safe message, optional details,
and a request ID. Internal database, parser, and Docker sources are logged for
diagnostics but are not returned or persisted as raw messages.

The daemon serves the same router over loopback TCP and a Unix-domain socket.
The typed client supports both transports. The API is intentionally polling
based: clients fetch application status and operation state instead of opening
event streams.

Supported resources and actions are:

- `GET /system/status`
- `GET /openapi.json`
- `GET /applications` and `GET /applications/{id}`
- `POST /applications` and `PUT /applications/{id}`
- `DELETE /applications/{id}`
- `POST /applications/plan` and `POST /applications/{id}/plan`
- `POST /applications/{id}/reconcile`
- `GET /applications/{id}/status`
- `GET /operations/{id}`

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
store rows and converts them to these DTOs at the API boundary. The essential
CLI workflow is documented in [`docs/piquelctl.md`](piquelctl.md). Authentication,
UI behavior, and additional transports are outside this product slice.
