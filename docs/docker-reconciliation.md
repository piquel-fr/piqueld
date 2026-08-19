# Docker reconciliation

Plan 06 connects `piqueld` directly to Docker Engine through Bollard. The daemon
requires an active Swarm manager at startup; when `docker.auto_initialize_swarm`
is enabled it may initialize an inactive engine as a single-node Swarm.

Prebuilt image tags are pulled and resolved to repository digests before the
application and operation are committed. Managed private overlay networks,
named local volumes, and replicated services carry instance/application
ownership labels. Same-name foreign resources block deployment. Services use
start-first updates, one-at-a-time parallelism, and pause on the first failure.
Ordinary application deletion removes services and the private network but
retains volumes.

Every update and removal re-inspects ownership at the Docker boundary. Service
and private-network removals additionally require the deterministic name to
match the application/service identity carried by the labels. A matching
service is canonicalized and returned as a no-op without issuing a Docker
update. Task failures expose only a structured state and exit code; raw daemon
messages are not persisted or returned because they may contain sensitive
runtime data.

Observation also records whether adapter-owned network, volume, and service
settings remain canonical. This prevents unsupported mounts, endpoint ports,
non-local volume drivers, or weakened restart/rolling-update policies from being
silently treated as converged. Immutable network/volume mismatches fail with a
sanitized configuration-conflict result instead of being mislabeled as an
ownership failure.

Swarm readiness requires exactly one ready, active, reachable manager. Ensure
requests are rejected at the adapter edge unless their ownership labels and
deterministic application-scoped names agree. Service convergence also waits for
Docker's rolling-update state to leave `updating`; running replacement tasks do
not by themselves complete an operation while Docker is still evaluating the
update. Docker-only endpoint modes, force-update counters, attachment options,
mount options, PID limits, and health-check timing are included in drift
classification rather than discarded during canonicalization.

Apply/delete requests wake the coordinator immediately. Docker events are hints
and are coalesced; a periodic full scan remains authoritative. Operation steps
are durable, idempotent, retry bounded transient Docker failures, and resume from
recovery after restart. Diagnostics stored in status/operation rows are stable
and sanitized rather than raw Docker daemon or task messages.
Retained-volume-only plans are informational and do not schedule recurring
reconcile operations.

Secret-bearing applications return `secret_lifecycle_unavailable` until Plan 07,
Git sources return `build_pipeline_unavailable` until Plan 08, and routed
applications return `routing_unavailable` until Plan 09. These workloads are not
silently deployed without the requested capability.

The privileged lifecycle test is opt-in. `just docker-test` starts a disposable,
privileged Docker-in-Docker daemon, waits for its private Unix socket, and runs
the test only against that socket. The container, socket, and inner daemon state
are removed when the command exits; the host daemon is never used as the test
target.

```text
just docker-test
```

The harness requires a Linux host with access to a local Docker-compatible daemon
through a Unix socket that permits privileged containers. `PIQUELD_DIND_IMAGE`
can override the pinned Docker-in-Docker image. The test initializes Swarm inside
the disposable daemon, creates/removes temporary services and networks, verifies
volume retention, and then removes the test volume explicitly.
