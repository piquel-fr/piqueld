# HTTP API and retry contract

The Plan 05 control-plane API is rooted at `/api/v1`. JSON responses use a
`{"data": ...}` envelope; list responses additionally contain `items` and an opaque
`next_cursor`. Errors use stable `code`, safe `message`, optional `details`, and a
request ID. The served OpenAPI document is available at `/api/v1/openapi.json` and
is checked against `docs/openapi-v1.json`.

Application create requires an `Idempotency-Key` header. Only a SHA-256 key digest
and the normalized request hash are stored in a durable binding alongside the
stable internal application and original generation-one operation identities. A
retry therefore returns the original result even after later replacements, without
rerunning source resolution. Reusing a key with different normalized intent is
rejected. Update, delete, and reconcile requests require the current
`expected_generation`; stale requests fail with HTTP 409.

Create and replacement accept structured JSON envelopes. Plan and apply endpoints
also accept a full TOML manifest with `Content-Type: application/toml`; TOML updates
carry `X-Expected-Generation`. Both paths pass through the same strict core parser,
validation, defaults, normalization, and hashing implementation.
Malformed or schema-incompatible TOML is a `400 toml_malformed`; a well-formed
manifest that fails semantic validation is a `422 manifest_validation_failed`.

Plan endpoints are read-only. Mutation endpoints return HTTP 202 and a durable
operation identity. Deletion never accepts `force` and operation plans retain named
volumes. The daemon requires its Docker runtime at startup. Pure unresolved previews
remain available without contacting that runtime. Replacement previews reuse only immutable resolutions whose
original source intent is unchanged, so ordinary configuration changes produce a
concrete desired/observed comparison without invoking a resolver. If observation is
unavailable, replacement planning returns `502 runtime_request_failed` instead of
treating missing runtime state as an empty observation.

Operation and application event endpoints use SSE. Events have durable/current-state
IDs, reconnect accepts `Last-Event-ID`, keepalives are sent every 15 seconds, and an
operation stream closes after its terminal event. Dropping the HTTP connection drops
the polling stream and its store references immediately.

If a reconnect cursor is no longer the current bounded durable state, the stream
first emits `replay_reset` with `{"reason":"bounded_replay_exhausted"}` and then
emits the current state. A cursor that still matches resumes without duplicating the
state event. Later state changes on the same connection are normal events, not replay
resets.

Reconcile creates its operation in the same durable store used by every other
mutation; the injected runtime boundary supplies preparation and observation.
Retrying reconcile for the same current generation while its operation remains
active returns the existing operation identity without repeating runtime
observation.

`piqueld-client` covers the complete Plan 05 endpoint surface over HTTP/TCP and Unix
sockets, including pagination, structured JSON and TOML create/replace/plan calls,
both SSE streams, status/capability discovery, OpenAPI retrieval, and operation
lookup. Dynamic IDs are encoded as single path segments.
