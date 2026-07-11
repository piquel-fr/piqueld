# Plan 01 — Workspace foundation and architecture contracts

## Goal

Create the smallest buildable repository skeleton on which every later plan can
work. Establish tooling, crate boundaries, configuration loading, error contracts,
and dependency decisions without implementing product behavior.

## Deliverables

- A Rust 2024 workspace with `crates/piqueld-core`, `crates/piqueld-client`,
  `apps/piqueld`, `apps/piquelctl`, and `apps/piqueld-ui`.
- Minimal compileable library/binary entry points and the daemon module tree from
  section 18.2 of the design.
- Workspace-level dependency/version/lint policy, `rustfmt.toml`, deny policy, and
  a minimal `flake.nix`/`flake.lock` development shell and checks.
- Strongly typed daemon bootstrap configuration matching section 20, with defaults,
  validation, and read-only TOML loading. No application configuration belongs in
  this file.
- Shared public error-code primitives in `piqueld-core`; `anyhow` is limited to
  binary startup boundaries.
- Structured tracing initialization and cancellation-token based graceful shutdown
  in the daemon skeleton.
- An ADR under `docs/architecture/` resolving how the official `libsql` crate and
  SQLx will coexist. Prove the choice with a tiny compile/test spike. The solution
  must retain embedded libSQL, explicit SQL migrations, and checked queries without
  opening two unsafe competing write paths.

## Work

1. Create workspace manifests, crate manifests, feature flags, and dependency
   direction. Add a dependency-boundary test or CI script proving that
   `piqueld-core` does not depend on Axum, libSQL/SQLx, Bollard, or Leptos.
2. Keep each executable intentionally small. The daemon can start, load config,
   initialize tracing, wait for SIGINT/SIGTERM, and exit cleanly; CLI/UI may only
   expose a version/build marker at this stage.
3. Model secrets in configuration as file/credential references, never inline
   values. Ensure debug output redacts credential paths if appropriate and never
   attempts to rewrite the source file.
4. Add workspace commands/documentation for formatting, linting, tests, and Nix
   checks. Seed `tests/` and `migrations/` without fake implementations.
5. Add a concise contributor architecture note describing allowed dependency flow:
   applications depend on client/core; daemon depends on core; core is pure.

## Verification

- `cargo fmt --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo test --workspace`
- `cargo test --doc --workspace`
- `nix flake check` (or document a concrete environmental blocker while keeping
  flake evaluation testable)
- Tests cover config defaults, malformed TOML, invalid listen/socket/registry
  settings, unknown config fields, and graceful cancellation.

## Done when

All workspace targets compile, the empty daemon starts and shuts down cleanly, the
database-stack ADR is backed by executable evidence, and later agents have stable
locations and commands. Do not add APIs, migrations, Docker calls, or UI behavior.

