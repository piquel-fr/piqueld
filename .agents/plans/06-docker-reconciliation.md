# Plan 06 — Docker Swarm adapter and reconciliation engine

## Goal

Connect the persisted control plane to Docker Engine directly, initialize/validate a
single-node Swarm, observe owned state, execute plans idempotently, wait for
convergence, and repair drift.

## Deliverables

- One shared Bollard connection/pool abstraction with a mockable Docker boundary.
- Startup checks and optional single-node Swarm initialization, including clear
  failures for unavailable Docker or a non-manager/incompatible Swarm.
- Observation adapters for services, tasks, overlay networks, named volumes, and
  relevant image state, canonicalized into Plan 03 models.
- Idempotent ensure/remove executors for private networks, named volumes, and
  replicated services with conservative rolling-update/failure policy.
- Prebuilt-image resolution to a registry digest before state commit/deployment.
- Reconciliation coordinator triggered after apply/delete, startup, Docker events,
  and periodic full scan.
- Convergence/status calculation, retry/backoff policy, operation-step journaling,
  cancellation, and restart continuation.

## Work

1. Apply ownership labels on every managed resource and validate all labels before
   update/delete. Treat a matching name without valid current-instance ownership as
   a blocking conflict.
2. Generate Docker API specs from core desired models at the adapter edge. Do not
   shell out to `docker`, `docker stack deploy`, or arbitrary host commands.
3. Use digest references for deployment. Store the resolved digest atomically with
   the operation as required by the apply workflow.
4. Configure replicated services, commands/args, environment, health checks,
   resource limits, private network, volume mounts, and start-first rolling updates
   where Docker permits. Secret mounts and public ingress are added by later plans.
5. Treat Docker event delivery as a hint; coalesce events and rely on periodic scans
   for recovery. Avoid reconciliation feedback loops.
6. Preserve the last healthy service on update failure, pause failed updates, and
   retain actionable sanitized task diagnostics.
7. On ordinary deletion remove services and the private network only after safe
   ordering; leave named volumes intact.

## Verification

- Unit tests with a fake Docker adapter for retry, partial failure, cancellation,
  event coalescing, ownership refusal, convergence, and idempotence.
- Docker integration tests (feature-gated/privileged) cover Swarm init, create,
  replica change, drift repair, restart mid-deploy, service deletion, and volume
  retention.
- Repeated reconcile against matching state produces no Docker mutations.

## Done when

A prebuilt-image application can be applied to a single-node Swarm, converges by
digest, repairs manual changes to owned resources, ignores unowned resources, and
survives daemon restart. Secret-bearing and routed services may be blocked with a
specific capability error until Plans 07 and 09, not silently deployed incompletely.

