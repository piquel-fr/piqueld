# Contract coverage

The maintenance audit found substantial existing coverage. The following suites
are the starting point for changes to each contract; add focused cases there
when changing behavior.

| Contract | Existing coverage |
| --- | --- |
| Manifest formats, defaults, rejection, canonical hashing | `piqueld-core/tests/manifests.rs` and manifest fixtures |
| Manifest variables: reference syntax, declarations, rendering per environment | `piqueld-core/src/manifest/template_tests/` |
| Read-only service files and repository file capture | `piqueld-core/tests/service_files.rs` |
| Git source validation and immutable resolution | `piqueld-core/tests/git_sources.rs` |
| Ownership, drift fields, cleanup gating, volume retention | `piqueld-core/tests/resource_planning.rs` |
| Links: validation, resolution, network projection, revocation | `piqueld-core/src/links.rs` unit tests, `tests/manifests.rs`, `tests/resource_planning.rs`, `piqueld/tests/fake_docker.rs`, and `docker_integration.rs` |
| Operation transitions and failure classification | Core lifecycle tests and `piqueld/tests/fake_docker.rs` |
| HTTP lifecycle, preconditions, force, receipts, pagination | `piqueld/tests/api_contract.rs` |
| Persistence, restart recovery, transaction rollback | `piqueld/tests/persistence.rs` and `sqlx_stack.rs` |
| Transport errors, cancellation, redirects, decoding | `piqueld-client/tests/transports.rs`, `generated_contract.rs`, and browser transport tests |
| Data and runtime directory privacy, daemon exclusivity | `piqueld/tests/data_dir.rs`, `runtime_dir.rs`, and `daemon_exclusivity.rs` |
| CLI commands, output, exit codes, connection profiles | `piquelctl/tests/cli.rs` and `profiles.rs` |
| Docker policy and complete observations | Docker adapter unit tests |
| Real Swarm health, drift repair, deletion, builds | `piqueld/tests/docker_integration.rs` through `just docker-test` |
| Dashboard and passkey flows in a real browser | `tests/playwright` through `just test-playwright` |

The additional audit cases check complete cleanup plans against reordered Engine
listings, including foreign resources, and compare real route response statuses
and media types against OpenAPI. Existing typed-client lifecycle tests cover
successful response decoding. The OpenAPI snapshot test also checks references.
These are focused conformance checks, not a general JSON Schema validator.
