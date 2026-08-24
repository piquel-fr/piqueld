# Resource compilation and planning

`piqueld-core` is the pure center of the runtime model. Image resolution is the
only mutable input required before compilation:

```text
image reference -> immutable repository digest -> desired Swarm resources
```

`preview_resolution` reports unresolved images without performing I/O.
`compile_application` produces backend-neutral desired private networks, named
volumes, and replicated services. Services carry environment, command, args,
mounts, health checks, resource limits, immutable images, and ownership labels.

Every owned resource is labeled with the control-plane instance, application, and
normalized specification hash. Services also carry their logical service name.
Deterministic resource names use the internal application ID, so editing the
user-facing name does not rename Docker resources.

The planner compares only fields that this product owns. Reconcile actions are
ordered as network and volume ensures, service ensures, convergence waits, and
cleanup of obsolete services and networks. Drift reasons name the differing
fields individually (for example `command`, `arguments`, `environment`). Same-name
foreign resources are blocking conflicts and are never mutated; obsolete owned
resources are only removed once the wanted infrastructure and services have
converged, and plans report `cleanup_deferred` diagnostics while removal is
pending.

Deletion removes owned services, waits for their absence, removes the private
network, and emits informational `retain_volume` actions for named volumes.
Volume data is deliberately not deleted by application deletion.

Plans are deterministic and safe to recompute from a fresh observation. The
daemon persists the generated operation steps, but later execution re-plans
against current Docker state so retries remain idempotent.
