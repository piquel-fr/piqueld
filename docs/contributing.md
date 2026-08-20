# Contributing and qualification

Use the repository's pinned Rust toolchain and run the authoritative check before
opening a change:

```console
just
```

The check is non-mutating and covers formatting, Clippy, workspace compilation,
tests, doctests, dependency policy, OpenAPI, and dependency boundaries. Focused
cross-layer checks are available as:

```console
./scripts/qualification.sh contracts
./scripts/qualification.sh nix
```

Build the UI explicitly with Trunk and run the browser smoke script when changing
the dashboard. Privileged tests are feature-gated and ignored:

```console
PIQUELD_DOCKER_ISOLATED=1 PIQUELD_DOCKER_DISPOSABLE=1 \
PIQUELD_DOCKER_SOCKET=/path/to/disposable/docker.sock \
cargo test -p piqueld --features docker-integration \
  --test docker_integration -- --ignored --test-threads=1
```

The environment must be disposable and isolated; the tests intentionally mutate
Swarm, services, networks, secrets, and volumes. Keep core free of adapter
dependencies, update OpenAPI/docs with public contract changes, add focused tests,
and preserve ownership, plaintext-safety, volume-retention, and transaction
invariants. Generated OpenAPI is updated only with `just generate-openapi`.
