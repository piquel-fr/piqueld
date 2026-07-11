# Plan 13 — NixOS deployment, authentication, recovery, and observability

## Goal

Turn the feature-complete application into a securely deployable single-host NixOS
service and harden its operational boundaries without expanding product scope.

## Deliverables

- Flake packages for daemon/CLI/UI assets and a NixOS module matching section 22,
  with generated read-only config, users/groups, state/runtime directories, Docker
  access, registry service/storage/trust, systemd units, and optional CLI install.
- Master key and bearer-token delivery through systemd credentials or protected
  files, never the Nix store. No firewall ports open automatically.
- Authentication middleware for Unix-socket permissions and a static bearer-token
  fallback; carefully constrained trusted Tailscale identity-header mode only when
  loopback/proxy preconditions are configured and spoofable headers are stripped.
- Request body/header/time limits, concurrency/backpressure, archive/build/log bounds,
  safe CORS/origin policy, and sanitized errors.
- `/health`, `/readiness`, system status/capabilities, and optional Prometheus metrics
  for the control plane (no time-series DB persistence).
- Robust startup/restart recovery for operations, reconciliation, builds, registry,
  Traefik, database, and Swarm; graceful shutdown drains or durably records work.
- Operator documentation for install, upgrades/migrations, key handling, Tailscale
  Serve, Cloudflare-to-Traefik only, recovery, and data/backup limitations.

## Work

1. Apply systemd hardening compatible with required Docker socket access and state
   paths. Document that Docker access is host-administrative and do not claim a
   stronger isolation boundary.
2. Validate Docker is available and single-node Swarm manager state is acceptable;
   auto-initialize only when configured. Do not manage additional nodes.
3. Run the registry on loopback/non-public interface with persistent storage. Ensure
   no option accidentally exposes registry, API, or Traefik dashboard publicly.
4. Readiness requires DB, Docker, Swarm manager, and required internal infrastructure;
   health only proves the process/event loop is alive. Avoid restart storms caused
   by transient readiness failures.
5. Add structured trace identifiers for application, service, operation, build, spec
   hash, and Docker service. Metrics must avoid names/secrets as high-cardinality or
   sensitive labels.
6. Validate configuration combinations for trusted proxy headers and fail closed.

## Verification

- NixOS VM tests for module evaluation, install/start, directories/permissions,
  credentials, Swarm init, registry, Unix socket, persistence, restart mid-operation,
  config changes, and no opened firewall ports.
- API security tests for missing/bad credentials, spoofed Tailscale headers, request
  limits, CORS/origin policy, and sanitized Docker/database errors.
- Readiness/health/metrics tests across dependency failures and recovery.
- Inspect Nix derivations/store and journal output for secret/token canaries.

## Done when

A fresh NixOS VM can install and safely operate piqueld with persistent state and
credentials, private management access, local registry/Swarm/Traefik readiness, and
predictable restart recovery.

