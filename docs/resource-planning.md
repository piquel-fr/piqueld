# Resource compilation and planning

`piqueld-core` separates mutable input resolution from runtime planning. Callers
resolve image tags to digest references and Git references to exact commits, build
and push Git sources, and supply the current immutable Swarm-secret generation.
`preview_resolution` reports any remaining resolve/build/push/secret work without
performing I/O. `compile_application` accepts only a complete `ResolutionSet` and
produces backend-neutral desired networks, volumes, secrets, and services.

Every application-owned resource carries `io.piqueld.managed=true`, the instance
ID, application ID, and spec hash. Services additionally carry their logical
service name. The shared ingress network is instance-owned. Public routes compile
to a closed set of host-only HTTP Traefik labels; routed services join both the
private application network and shared ingress network.

The pure planner compares only fields piqueld owns. Collection/map ordering and
unrelated runtime labels are ignored, while missing or changed owned fields emit
idempotent `ensure` actions. Actions are dependency ordered: networks, volumes,
secrets, services, convergence waits, then obsolete services, secrets, and private
networks. Same-name unowned resources are blocking conflicts and are never mutated;
other foreign resources are diagnostics only.

Secret generations are immutable. An old generation is removed only after all
desired services have adopted the current generation, converged, and Docker reports
the old generation unused. Failed updates block that cleanup. Application deletion
removes owned services, unused secrets, and private networks, but emits explicit
non-mutating `retain_volume` actions for owned named volumes. Volume removal requires
a separate future destructive operation.
