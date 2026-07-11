# Workspace dependency flow

`piqueld-core` is the pure center of the workspace. It owns domain and public error
contracts and must not depend on Axum, libSQL/SQLx, Bollard, or Leptos.

`piqueld-client` depends on core and will own typed HTTP client behavior. `piquelctl`
and `piqueld-ui` depend on the client and core contracts. The `piqueld` daemon depends
on core directly; persistence, Docker, API, and other
adapters remain internal daemon modules. Applications never become dependencies of
libraries.

Run `./scripts/check-dependency-boundaries.sh` after changing manifests. An
equivalent metadata-based guard is part of the Nix flake checks.
