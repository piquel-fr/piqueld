# Plan 03 — Deterministic resource compilation and pure planning

## Goal

Translate normalized application intent into a backend-neutral resolved resource
model, compare it with observed owned resources, and emit a deterministic,
inspectable plan. Keep Docker I/O out of `piqueld-core`.

## Deliverables

- Resolved application/source types carrying immutable Git commits, image digests,
  registry references, normalized resource names, and spec hash.
- Desired and observed models for networks, volumes, secrets, services, tasks, image
  references, health/convergence, and relevant Traefik labels.
- Deterministic service/network/volume/secret and Traefik-label compilation.
- Typed `Plan`, ordered `PlanAction`, action reason, risk/destructive classification,
  and stable human/JSON representations.
- Pure planning for create, update, drift repair, deletion, wait/convergence, secret
  generation adoption, and retained volumes.
- Ownership-label validation incorporating managed flag, instance ID, application
  ID, service name where relevant, and spec hash.

## Work

1. Separate input resolution from planning. The planner consumes a resolved desired
   state and never contacts Git, a registry, or Docker.
2. Define semantic comparison so Docker defaults, map ordering, and API noise do not
   cause endless updates. Compare only fields piqueld owns.
3. Order actions by dependencies: shared/private networks and volumes; secrets;
   services; convergence; obsolete service/network/secret cleanup. Volume deletion
   is never included in ordinary application deletion.
4. Unknown and foreign resources must yield ignore/conflict diagnostics, never a
   mutation. A same-name unowned resource is a blocking conflict.
5. Compile host-only HTTP routes to deterministic Traefik labels and attach routed
   services to both private and shared ingress networks. Do not admit arbitrary
   labels.
6. Model resolution/build actions explicitly so preview plans can say that values
   remain unresolved while apply plans carry immutable results.
7. Add plan summaries suitable for CLI/UI without losing typed detail.

## Verification

- Table/golden tests for resource compilation and all plan transitions.
- Property: matching desired/observed state produces an empty mutation plan.
- Property: planning is deterministic and does not mutate its inputs.
- Tests cover partial operations, service drift, replica/environment/image changes,
  missing secrets, failed updates, unowned name collisions, application deletion,
  and volume retention.
- Traefik and ownership labels have exact snapshot tests.

## Done when

Given resolved desired state plus synthetic observed state, the core crate produces
the complete safe action sequence agents will later execute, with no runtime I/O.

