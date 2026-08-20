# piqueld prototype implementation plan index

These plans decompose `.agents/prototype-1.md` into sequential, independently
verifiable increments. Implement them in numeric order. Each agent should read the
design specification, this index, its assigned plan, and the completion notes or
commits from all earlier plans before changing code.

An implementing agent owns only its numbered increment plus defects it uncovers in
earlier increments that block that work. It should finish the plan's verification,
document any deliberate design deviation, and leave a short handoff describing
changed public contracts, migrations, feature flags, and tests that could not run.

| Plan | Increment | Primary outcome |
| --- | --- | --- |
| 01 | Workspace foundation | Buildable Rust/Nix workspace and enforced architecture boundaries |
| 02 | Core application model | Strict manifests, validation, normalization, hashing, and names |
| 03 | Resource compilation and planning | Pure desired/observed comparison with deterministic actions |
| 04 | Persistence and operation journal | Migrations, transactional repositories, and crash-aware operations |
| 05 | HTTP API and typed client | Application/plan/operation API, OpenAPI, SSE, and client |
| 06 | Docker reconciliation | Swarm bootstrap, observation, execution, convergence, and drift repair |
| 06A | Simplification and consistency | Remove speculative surface area and make the Plan 06 product coherent |
| 06B | Usable CLI | Essential `piquelctl` workflows for operating the Plan 06 product |
| 06C | Basic web dashboard | Read-only Leptos/WASM visibility into daemon and application state |
| 07 | Secret lifecycle | Encrypted logical secrets and immutable Swarm-secret rotation |
| 08 | Build and registry pipeline | Git resolution, BuildKit builds, registry push, digest deployment |
| 09 | Traefik and runtime visibility | Ingress infrastructure, routes, status, logs, and events |
| 10 | State import and export | Portable application exports and transactional control-plane archives |
| 11 | CLI | Complete `piquelctl` operator workflow |
| 12 | Web UI | Minimal structured Rust/WASM administration UI |
| 13 | NixOS, security, and operations | Deployable module, auth, limits, recovery, health, and metrics |
| 14 | End-to-end qualification | Integration/VM/security tests, CI, docs, and acceptance runbook |

Plans 06A–06C form the new usable-product stack. Plans 07–14 remain the feature
specifications for the later stack, but their existing pull requests must be
semantically transplanted onto 06C rather than mechanically rebased. Follow
`rebase-07-14-onto-product-stack.md` for that operation. Preserve the existing PR
numbers and review history while adapting their implementation to the simpler
foundations introduced by 06A–06C.

Cross-cutting rules for every plan:

- Preserve the single-process, single-node-Swarm prototype boundary and do not
  implement deferred features.
- Keep `piqueld-core` free of Axum, database, Docker, and UI dependencies.
- Never expose secret plaintext in logs, errors, serialization, browser storage,
  exports, labels, or command arguments.
- Never mutate or delete Docker resources unless ownership labels identify the
  current piqueld instance. Retain named volumes by default.
- Prefer deterministic pure transformations and idempotent `ensure` operations.
- Add focused tests with every increment and leave the whole workspace green.
- Do not silently weaken an acceptance criterion when an upstream crate or Docker
  API differs from the design. Record the decision and update interfaces/tests.
- Do not create placeholder endpoints or report unavailable capabilities as healthy;
  introduce a mockable boundary for tests and expose honest production capability
  state until the implementing plan lands.
