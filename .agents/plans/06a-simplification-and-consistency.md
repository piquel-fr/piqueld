# Plan 06A — Simplify and make the Plan 06 product consistent

## Goal

Turn the completed Plan 06 implementation into a small, honest product foundation
before adding user interfaces. Remove contracts, persistence, runtime machinery,
and documentation that exist only for unimplemented later plans. Keep the working
single-node Docker Swarm path clear and internally consistent.

This product has not been deployed. Baseline migration files may be edited directly;
do not preserve upgrade compatibility for schemas that no user has run.

## Supported product after this plan

An application manifest supports:

- a prebuilt container image;
- replica count, environment, command, and arguments;
- health checks and resource limits; and
- named volumes and service mounts.

The daemon can validate, plan, apply, inspect, reconcile, and delete these
applications through its public API over a protected Unix socket or loopback TCP.
It resolves images to digests, owns only labelled Docker resources, retains named
volumes on deletion, and reports durable operation and reconciliation state.

Git sources, builds, registries, secrets, published ports, routes, Traefik, logs,
state transfer, authentication, CLI, and UI are deliberately absent until later
plans. Unsupported fields must not remain as accepted-but-blocked product surface.

## Deliverables

- A manifest, planner, API, persistence model, and daemon that expose only the
  supported Plan 06 workload.
- One authoritative representation for application and operation state at each
  layer, with explicit conversion at boundaries.
- A smaller persistence API and fresh-install schema with no future-feature tables.
- Polling-friendly HTTP/client contracts without unused event-stream machinery.
- Consistent validation commands and current architecture/product documentation.
- Focused tests for retained behavior, with obsolete tests removed rather than
  preserved as regression tests for deleted features.

## Work

### 1. Remove speculative manifest and planning surface

1. Remove Git source variants and their validation. A deployable source is a
   prebuilt image reference only.
2. Remove routes, published ports, secret declarations/references, and secret
   resolution from manifests and normalized desired state.
3. Remove planner actions that cannot execute in Plan 06, including Git resolution,
   image build/push, and secret generation. Keep image resolution only if it is part
   of the working prebuilt-image apply path; document whether it is preview-only or
   executable.
4. Delete empty or placeholder build, registry, proxy/Traefik, and secrets modules.
   Do not leave module shells in anticipation of Plans 07–09.
5. Remove configuration used only by those features, including registry and
   Traefik configuration, credential references, and parallel-build limits.

### 2. Reduce state and scheduling concepts

1. Remove operation kinds that have no independent working operation, such as
   standalone build or deploy kinds, when apply already owns the lifecycle.
2. Remove application states such as resolving or building if they are not
   authoritative, observable states of the supported product.
3. Define and document which component owns each state transition. Durable operation
   state is authoritative for command progress; observed Docker state is
   authoritative for runtime convergence.
4. Remove the build repository, build records/table, and build-only scheduling
   semaphore. There should be one clear concurrency owner for reconciliation.
5. Prefer a concrete coordinator over an interface with one implementation. Retain
   traits only at genuine external/test seams, notably the Docker boundary, runtime
   boundary, and operation handler.

### 3. Simplify persistence

1. Make single-implementation repository methods inherent `SqliteStore` methods
   unless a real interchangeable boundary exists.
2. Keep transactions around invariants, not around repository abstraction for its
   own sake.
3. Edit the baseline migration files to describe the resulting schema directly.
   Remove future-feature columns, tables, indexes, and migration tests. Renumber or
   consolidate migrations when doing so makes the fresh schema easier to understand.
4. Test a fresh database, constraints, application generation updates, operation
   recovery, and deletion/volume-retention metadata. Do not add upgrade tests from
   the abandoned internal schema.

### 4. Clarify API, client, and errors

1. Keep persistence types private to the daemon. Convert store records into public
   DTOs at the API/service boundary; the store must not return client DTOs.
2. Use one public error envelope and status-code mapping. Remove duplicate public
   error/error-code hierarchies unless they represent a real client contract.
3. Preserve detailed SQLx, I/O, and Docker error sources internally. Sanitize only
   at the public API and durable-operation diagnostic boundaries.
4. Remove custom error equality implementations if tests can assert meaningful
   variants/fields instead.
5. Separate transport-neutral DTOs and request/response types from the native HTTP
   transport implementation so a later WASM client can reuse them. Do not add a new
   contracts crate unless the existing crate boundaries make that strictly
   necessary.
6. Remove application/operation SSE endpoints, current-state hashing, replay/reset
   behavior, and the custom client SSE decoder. Plans 06B and 06C use bounded
   polling. Preserve ordinary pagination and explicit operation lookup.
7. Keep both the Unix-socket and loopback-TCP transports. Consolidate duplicated
   startup/router logic, and retain restrictive Unix-socket permissions.

### 5. Make validation and dependency boundaries obvious

> **Deviation (recorded):** the shipped `just` regenerates the OpenAPI snapshot
> before validating (`default: generate-openapi validate`), and the explicit
> mutating command is `just generate-openapi`. `just validate` remains the
> canonical non-mutating entry point promised below; README documents both.

1. Make `just` the canonical, non-mutating local validation entry point. It should
   run strict formatting checks, Clippy with warnings denied, focused tests, docs,
   dependency/license checks, architecture-boundary checks, and an OpenAPI snapshot
   check as applicable. *(Completed with the accepted deviation recorded above:
   the default `just` recipe regenerates the OpenAPI snapshot before validating;
   `just validate` is the non-mutating entry point.)*
2. Keep generated-output mutation behind an explicit `just generate` command.
   *(Completed as `just generate-openapi`, per the same recorded deviation.)*
3. Keep privileged Docker integration checks and Nix evaluation in explicit
   commands such as `just docker-test` and `just nix-check` when they cannot be part
   of the default portable check.
4. Remove duplicated dependency declarations/check logic between Cargo, Nix, shell,
   and CI where one source can be authoritative.
5. Audit the remaining dependency graph. Removing future features should remove
   their transitive dependencies as well.

### 6. Rewrite documentation for the product that exists

1. Replace plan-era or future-tense README text with a concise description of the
   working daemon, supported manifest fields, transports, and development workflow.
2. Add a supported/deferred capability table so omission is intentional and easy to
   review.
3. Document the architecture and the few retained abstraction boundaries.
4. Document image digest resolution, ownership labels, polling, drift repair,
   operation recovery/retention, deletion ordering, and named-volume retention.
5. Update manifest examples, API examples, OpenAPI output, migration notes, and
   reconciliation documentation together with their code.
6. Do not present curl as the intended long-term operator experience; Plan 06B adds
   that workflow. A small diagnostic example is acceptable.

## Explicitly out of scope

- Any feature assigned to Plans 07–14.
- Authentication, remote/public exposure, packaging, or multi-node/multi-user work.
- A CLI or browser UI.
- New generic frameworks, plugin systems, event buses, contract crates, or repository
  interfaces introduced only to prepare for unknown future needs.
- Compatibility migrations for unreleased schemas.

## Verification

- Run the canonical `just` validation and ensure it does not modify tracked files.
- Run fresh-database persistence tests and API/OpenAPI snapshot tests.
- Run focused fake-Docker reconciliation tests for create, update, idempotence,
  drift, ownership refusal, recovery, deletion ordering, and volume retention.
- Run the privileged Docker integration suite when the environment supports it;
  otherwise record exactly what could not run.
- Search source, tests, examples, configuration, and docs for removed concepts. Any
  remaining mention must be an explicit deferred-feature statement or later plan.
- Confirm the public client still exercises both Unix and TCP transports.

## Done when

The repository describes and implements one coherent Plan 06 product: a fresh
database and daemon can apply and reconcile a supported prebuilt-image application,
the public API/client can observe and operate it, unsupported future fields are not
accepted, and no unused subsystem is kept merely for the old PR stack. The diff is
primarily a deletion/consolidation and leaves a clear base for the CLI.

## Handoff

Record the final supported manifest shape, API/OpenAPI changes, migration rewrite,
removed dependencies, retained test seams, and validation commands. Call out any
deferred concept that could not be removed and explain the concrete reason.
