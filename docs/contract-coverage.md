# Contract coverage

The maintenance audit found substantial existing coverage. The following suites
are the starting point for changes to each contract; add focused cases there
when changing behavior.

| Contract | Existing coverage |
| --- | --- |
| Manifest formats, defaults, rejection, canonical hashing | `piqueld-core/tests/manifests.rs` and manifest fixtures |
| Git source validation and immutable resolution | `piqueld-core/tests/git_sources.rs` |
| Ownership, drift fields, cleanup gating, volume retention | `piqueld-core/tests/resource_planning.rs` |
| Operation transitions and failure classification | Core lifecycle tests and `piqueld/tests/fake_docker.rs` |
| HTTP lifecycle, preconditions, force, receipts, pagination | `piqueld/tests/api_contract.rs` |
| Persistence, restart recovery, transaction rollback | `piqueld/tests/persistence.rs` and `sqlx_stack.rs` |
| Transport errors, cancellation, redirects, decoding | `piqueld-client/tests/transports.rs` and browser transport tests |
| Docker policy and complete observations | Docker adapter unit tests |
| Real Swarm health, drift repair, deletion, builds | `piqueld/tests/docker_integration.rs` through `just docker-test` |

The additional audit cases check complete cleanup plans against reordered Engine
listings, including foreign resources, and compare real route response statuses
and media types against OpenAPI. Existing typed-client lifecycle tests cover
successful response decoding. The OpenAPI snapshot test also checks references.
These are focused conformance checks, not a general JSON Schema validator.
