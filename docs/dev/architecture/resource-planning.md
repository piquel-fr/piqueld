# Resource compilation and planning

`piqueld-core` contains deterministic manifest validation, resource compilation,
planning, and shared lifecycle records. It performs no Docker or database I/O.
Manifest input keeps raw strings for aggregate validation. Normalized state
exposes immutable service and mount views with typed logical names. Deserializing
normalized state revalidates all manifest rules and restores canonical ordering.

```text
manifest + resolved image digests -> desired resources
                   desired resources + observation -> plan
```

Desired resources carry ownership labels and deterministic names based on the
internal environment ID. Services additionally carry their logical service name.
The manifest remains portable; runtime names and resolved image references are
separate from its user-facing fields.

A reconcile plan ensures networks and volumes, applies services, waits for
convergence, and removes obsolete services and networks. Cleanup waits until the
wanted infrastructure and services are ready. The planner compares the fields
piqueld owns and reports blocking ownership or immutable-configuration conflicts.
Foreign resources are never mutated.

A deletion plan removes owned services and networks. Named volumes appear as
informational retention actions and remain available after environment deletion;
deleting a preview also removes the volumes it created.

[Links](docker-reconciliation.md#link-networks) are resolved from every
environment's active target (`piqueld_core::links`) and projected onto a fresh
copy of the target each time it is planned, like ingress: the importer gains
the link network and the exporter's imported services join it under their
`<service>.<application>` alias. They are never part of a prepared target.

Deployments run before-rollout jobs to completion before planning the rollout,
so only plan previews list them, as `run_job` actions.

Plans describe current work and are safe to recompute. The controller executes
an action and observes again; plans are not persisted as operation steps.
A plan preview is an explanation of the current proposed transition, not a promise
that Docker state will remain unchanged until apply.

Plan previews include redacted differences between the last deployment and
proposed manifest fields. They show fresh source-resolution requirements, even
for unchanged image references. Preview does not resolve or build sources, so it
cannot show every concrete Docker mutation that deployment will require. Runtime
actions in previews have sensitive configuration redacted and are never used for
execution.
