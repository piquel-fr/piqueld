# Plan 09 — Traefik ingress and runtime visibility

## Goal

Complete public HTTP routing and operator runtime visibility while maintaining the
strict separation between Cloudflare Tunnel, Traefik, and the control-plane API.

## Deliverables

- Idempotently managed shared ingress overlay network and Traefik Swarm service with
  current-instance ownership labels and a pinned/configurable image digest.
- Routed application services attached to ingress plus private networks and carrying
  only the generated labels from Plan 03.
- Host-route conflict detection across all applications and route verification.
- Runtime application status derived from Swarm service/task/update/health state,
  with useful degraded/failed diagnostics.
- Application/container log retrieval and bounded follow stream through API, SSE,
  OpenAPI, and typed client; multiplexed records include service/task/time metadata.
- Documentation/example for connecting externally managed `cloudflared` to the
  Traefik origin port without exposing internal administrative endpoints.

## Work

1. Manage Traefik as internal infrastructure, not as a user application. Disable its
   dashboard/API exposure, Docker-by-default discovery, and arbitrary labels.
2. Ensure only services with routes join ingress. Generate deterministic router and
   service identifiers; escape host rules safely and use the intended internal port.
3. Make origin binding configurable and safe for loopback/external cloudflared
   topology. Never configure Cloudflare accounts, DNS, credentials, or Access.
4. Observe Traefik and ingress readiness as internal infrastructure status. Block or
   degrade routed deployments when the route cannot function.
5. Bound log query duration/size and follower buffering; handle task replacement,
   client lag, cancellation, and Docker errors. Strip ANSI only for presentation,
   preserving raw safe text where useful.
6. Keep `/api`, Docker, registry, and Traefik admin endpoints out of public routes by
   construction and tests.

## Verification

- Exact label and network attachment tests, including hostname escaping and global
  route collision.
- Docker integration tests deploy a small HTTP service, reach it through Traefik,
  scale replicas, update it, and verify status/log streams across task replacement.
- Security tests prove arbitrary Traefik labels and control-plane/internal host routes
  cannot be introduced through manifests.
- Reconcile recreates missing owned ingress resources and refuses unowned collisions.

## Done when

A host route reliably reaches the correct Swarm service through managed Traefik, and
operators can inspect live status and logs without exposing control-plane internals.

