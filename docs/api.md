# HTTP API and retry contract

The Plan 05 control-plane API is rooted at `/api/v1`. JSON responses use a
`{"data": ...}` envelope; list responses additionally contain `items` and an opaque
`next_cursor`. Errors use stable `code`, safe `message`, optional `details`, and a
request ID. The served OpenAPI document is available at `/api/v1/openapi.json` and
is checked against `docs/openapi-v1.json`.

Application create requires an `Idempotency-Key` header. The key is converted into
a stable internal application identity, so a retry after a lost response returns
the original generation-one operation. Reusing a key with different normalized
intent is rejected. Update, delete, and reconcile requests require the current
`expected_generation`; stale requests fail with HTTP 409.

Create and replacement accept structured JSON envelopes. Plan and apply endpoints
also accept a full TOML manifest with `Content-Type: application/toml`; TOML updates
carry `X-Expected-Generation`. Both paths pass through the same strict core parser,
validation, defaults, normalization, and hashing implementation.

Plan endpoints are read-only. Mutation endpoints return HTTP 202 and a durable
operation identity. Deletion never accepts `force` and operation plans retain named
volumes. Until Plan 06 supplies Docker source resolution and execution, capability
discovery reports those features unavailable and apply/reconcile requests return
HTTP 503 rather than persisting fabricated resolved state. Pure unresolved previews
remain available.

Operation and application event endpoints use SSE. Events have durable/current-state
IDs, reconnect accepts `Last-Event-ID`, keepalives are sent every 15 seconds, and an
operation stream closes after its terminal event. Dropping the HTTP connection drops
the polling stream and its store references immediately.
