# Plan 08 — Git, BuildKit, local registry, and digest pipeline

## Goal

Resolve Git sources reproducibly, build Dockerfiles in isolation, push outputs to a
configurable local OCI registry, and deploy only immutable digests while exposing
durable progress and logs.

## Deliverables

- Registry configuration/client and readiness checks; deterministic image repository
  paths and content-derived build keys.
- HTTPS Git reference resolution and isolated checkout with `gix`, resolving mutable
  references to exact commits. Token credentials use protected providers and are
  redacted; SSH may remain unsupported with a clear error.
- Safe build context creation in Rust with context/Dockerfile containment, ignore
  handling, symlink defense, maximum size, cleanup, and timeout.
- BuildKit integration (not a shell pipeline), local tag, registry push, manifest
  digest resolution/verification, and digest-based resolved state.
- Bounded build concurrency integrated with Plan 04's scheduler; durable build rows,
  operation steps, cancellation, restart behavior, and streamed/redacted logs.
- Build API/OpenAPI/client extensions for status and SSE/log consumption.

## Work

1. Define build identity from commit, context, Dockerfile, non-secret build args,
   target, platform, and builder configuration. Cache only exact identities.
2. Reject credentials in repository URLs and reject unsafe context paths. Prevent
   archive traversal and context escape through symlinks.
3. Do not pass secrets as ordinary build args. Either implement BuildKit secret
   mounts safely with separate protected input or return an explicit unsupported
   capability for build secrets.
4. Capture logs with sequence/timestamps and bounded retention; redact known tokens
   and never include process environments or credential-bearing URLs.
5. Verify the pushed digest from the registry response/manifest and persist it before
   asking reconciliation to update the service.
6. On daemon restart, safely retry resolution/push or mark an interrupted local build
   for a fresh isolated rebuild; never claim an unverified image succeeded.
7. Implement only conservative cleanup that proves no current resolved application
   references an image. Aggressive registry garbage collection remains out of scope.

## Verification

- Unit tests for build-key stability, URL redaction, context bounds/traversal,
  cancellation, timeout, log ordering, and concurrency limits.
- Local registry integration test builds a fixture repository, pushes, resolves the
  digest, and deploys that digest to Swarm.
- Branch movement produces a new stored commit/build; unchanged identity reuses only
  a verified result.
- Canary credentials never occur in captured logs, errors, tags, or rows.

## Done when

A Git/Dockerfile application progresses through resolve/build/push/deploy with an
exact stored commit and registry digest, observable logs, bounded resources, and
safe restart behavior.

