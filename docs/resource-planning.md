# Resource compilation and planning

`piqueld-core` separates mutable input resolution from runtime planning. Callers
resolve image tags to digest references and Git references to exact commits, build
and push Git sources, and supply the current immutable Swarm-secret generation.
`preview_resolution` reports any remaining resolve/build/push/secret work without
performing I/O. `compile_application` accepts only a complete `ResolutionSet` and
produces backend-neutral desired networks, volumes, secrets, and services. Image
digests must resolve the requested repository (including canonical Docker Hub
names), Git build digests must resolve the built registry repository, and secret
generation names are deterministic and application-scoped.
Resolved registry and digest references are rejected if they contain credentials,
URL syntax, whitespace, or other non-image-reference data, keeping resolver secrets
out of the serializable desired model.

Every application-owned resource carries `io.piqueld.managed=true`, the instance
ID, application ID, and spec hash. Services additionally carry their logical
service name. The shared ingress network is instance-owned. Public routes compile
to a closed set of host-only HTTP Traefik labels, including an explicit
`traefik.swarm.network` selection; routed services join both the private application
network and shared ingress network.

The pure planner compares only fields piqueld owns. Collection/map ordering for
ports, mounts, secrets, networks, and labels, plus unrelated runtime labels, is
ignored; changed owned fields emit idempotent `ensure` actions. Actions are
dependency ordered: networks, volumes, secrets, all service ensures, all
convergence waits, then obsolete services, private networks, and secrets. Cleanup
is deferred until infrastructure exists and every desired service has converged;
obsolete service removals have their own absence barrier before network cleanup.
Same-name unowned resources are blocking conflicts and are never mutated; other
foreign resources are diagnostics only. Service deletion additionally requires a
valid logical-service ownership label whose deterministic name matches the observed
resource.

Secret generations are immutable. An old generation is removed only after all
desired services have adopted the current generation, converged, and Docker reports
the old generation unused. Failed updates block that cleanup. Application deletion
removes owned services, waits for their absence, then removes owned secrets and
private networks. An in-use secret also has an explicit unused barrier before its
removal, so references outside the removed service set cannot be bypassed. Deletion
emits non-mutating `retain_volume` actions for owned named volumes. Volume removal
requires a separate future destructive operation.
