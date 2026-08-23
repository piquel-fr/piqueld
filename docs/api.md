# HTTP API

The versioned API is rooted at `/api/v1`. JSON responses use a `data` envelope;
list responses contain `items` and an opaque `next_cursor`. Errors use one
stable envelope with a machine-readable code, safe message, optional details,
and a request ID. Internal database, parser, and Docker sources are logged for
diagnostics but are not returned or persisted as raw messages; malformed
request bodies additionally log the safe field path that was rejected.

The daemon serves the same router over loopback TCP and a Unix-domain socket.
The typed client supports both transports. The API is intentionally polling
based: clients fetch application status and operation state instead of opening
event streams.

Because the API is unauthenticated, TCP requests whose `Host` is not loopback
(`localhost`, `127.0.0.1`, `[::1]`) are rejected with 403 `host_not_allowed`;
this blocks DNS-rebinding browsers. Unix-socket requests carry no host and are
always accepted. Unsupported methods on known routes answer 405 with an `Allow`
header.

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
well. Requests carrying more than one `Idempotency-Key` header value are
rejected. Only the key's SHA-256 digest and the normalized request hash are
stored; repeating a matching mutation replays the original operation identity,
a cancelled or failed bound delete/replace is resurrected instead of replayed
dead, and reusing a key for a different canonical spec conflicts with 409.
Bindings live in the operation journal and are deleted together with their
operation by the store's retention pruning, so key reuse after that horizon
starts a new operation instead of replaying. The daemon does not yet schedule
that pruning pass itself; terminal history is retained until it is wired up. All mutations use optimistic generation checks (JSON bodies carry
`expected_generation`; deletion requires one) and return a durable operation
identity with HTTP 202. Polling `GET /operations/{id}` exposes safe operation
and step diagnostics.

Create, replace, and plan accept structured JSON or a complete TOML manifest with
`Content-Type: application/toml` (also `text/toml`). TOML replacement carries
`X-Expected-Generation`. Plan endpoints perform no durable mutation and report
image resolution requirements when the runtime cannot yet produce a concrete
desired plan; a blocked replace plan answers 409 with its blocking plan
diagnostics in the error envelope's `details`.

The public client contracts live in `piqueld-client`; persistence uses internal
store rows and converts them to these DTOs at the API boundary. The essential
CLI workflow is documented in [`docs/piquelctl.md`](piquelctl.md). Authentication,
UI behavior, and additional transports are outside this product slice.
