# Resource compilation and planning

`piqueld-core` contains deterministic manifest validation, resource compilation,
planning, and shared lifecycle records. It performs no Docker or database I/O.
Manifest input and normalized state share the same service, mount, health-check,
and resource-limit types.

```text
manifest + resolved image digests -> desired resources
                   desired resources + observation -> plan
```

Desired resources carry ownership labels and deterministic names based on the
internal application ID. Services additionally carry their logical service name.
The manifest remains portable; runtime names and resolved image references are
separate from its user-facing fields.

A reconcile plan ensures networks and volumes, applies services, waits for
convergence, and removes obsolete services and networks. Cleanup waits until the
wanted infrastructure and services are ready. The planner compares the fields
piqueld owns and reports blocking ownership or immutable-configuration conflicts.
Foreign resources are never mutated.

A deletion plan removes owned services and networks. Named volumes appear as
informational retention actions and remain available after application deletion.

Plans describe current work and are safe to recompute. The controller executes
an action and observes again; plans are not persisted as operation steps.
A preview is an explanation of the current proposed transition, not a promise
that Docker state will remain unchanged until apply.

Preview responses also include redacted differences between accepted and proposed
manifest fields. Unchanged service image references reuse active digests; new or
changed references remain explicit resolution requirements. Runtime actions in
previews have sensitive configuration redacted and are never used for execution.
