# Plan 05 — Versioned HTTP API, OpenAPI, SSE, and typed client

## Goal

Expose the domain and persistence capabilities through one stable public HTTP/JSON
API and consume that API through `piqueld-client`. The daemon remains the only
process with store access.

## Deliverables

- Axum router for system status/capabilities; application list/create/get/update/
  delete-intent; create/update plan; reconcile request; status; and operation get.
- Consistent envelopes/pagination where applicable and structured error bodies with
  stable codes and details.
- OpenAPI generated and served from the daemon; checked-in snapshot catches drift.
- SSE operation/application event endpoints with IDs, keepalives, replay from a
  bounded durable/current-state source, lag behavior, and clean cancellation.
- Typed async `piqueld-client` supporting TCP base URLs and local Unix sockets.
- Request IDs, tracing spans, content-type enforcement, and sanitized internal
  errors. Auth and final request limits arrive in Plan 13, but middleware seams must
  exist now.

## Work

1. Define transport DTOs separately from store rows. Reuse public core schema types
   without leaking Docker or encryption internals.
2. Route both structured JSON application updates and full parsed manifest requests
   through the same application service and normalization path.
   Define an injected source-resolution/execution boundary so API contract tests can
   use deterministic fakes. In the real daemon, an unavailable resolver/executor must
   produce an honest capability/unavailable result until Plan 06 supplies it; never
   persist fabricated resolved values.
3. Plan endpoints are read-only: they validate/compare and do not persist desired
   state or schedule work. Apply endpoints require expected generation on updates.
4. Return `202 Accepted` plus operation identity for asynchronous mutation. Ensure
   idempotency/retry semantics are documented; introduce request idempotency keys if
   needed to prevent duplicate create operations.
5. Make delete behavior explicitly retain volumes and reject unsupported force
   semantics.
6. Build the client by hand from shared DTOs or generated schema, but do not let the
   UI/CLI bypass it with database calls.

## Verification

- Router tests exercise success, validation failure, not found, generation conflict,
  name collision, plan purity, malformed JSON/TOML, and method/content-type errors.
- Contract tests run the typed client against an in-process server over TCP and Unix
  socket.
- SSE tests cover reconnect, event IDs, lag, terminal events, and disconnect cleanup.
- OpenAPI validates and includes every implemented endpoint/error schema.

## Done when

An API client can exercise application, plan, and operation contracts against
injected deterministic boundaries and watch durable progress. The real daemon
reports unavailable runtime capabilities honestly until Plan 06 connects Docker. Do
not publish stub endpoints for secrets, builds, logs, state archives, or UI-only
behavior; later plans add those when functional.
