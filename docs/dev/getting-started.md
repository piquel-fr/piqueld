# Getting started

Use the Rust toolchain pinned in `rust-toolchain.toml`, with its
`wasm32-unknown-unknown` and `x86_64-unknown-linux-musl` targets: daemon
builds with `embedded-ui` embed a dashboard built for the first, and every
daemon build embeds a
[health probe](architecture/docker-reconciliation.md#http-health-probe) built for the
second. Nix is optional; `nix develop`
provides a reproducible development shell and the flake contains deployable
packages. macOS supports the CLI on Apple Silicon: use
`just validate-cli` for CLI development and `nix build .#cli` for the Apple
Silicon Nix package.
See [macOS support](../user/reference/cli.md#macos-support) for development and
connecting to a Linux daemon. The full Linux validation command is:

```console
just
```

`just` formats sources, then checks lints, compilation, tests, documentation
tests, dependency licenses, dependency boundaries, browser compilation, the
Playwright browser tests (run `just setup-playwright` first), the
[documentation](documentation.md#building-and-checking), and the freshness
of the checked-in OpenAPI specification, generated client, and manifest JSON
Schema. It does not modify generated files. Regenerate all three artifacts in
dependency order with:

```console
just generate
```

See [automated validation](validation.md) for the CI jobs and
[contract coverage](contract-coverage.md) for where tests live.

Client generation is implemented in Rust with Progenitor. Ordinary Cargo builds
use the checked-in client without compiling the generator. See
[client generation](https://github.com/piquel-fr/piqueld/blob/main/tools/client-codegen/README.md) for the shared contract
mappings and the handwritten requests.

The optional privileged Docker qualification uses an isolated Docker-in-Docker
daemon:

```console
just docker-test
```

Both `just test` and `just docker-test` use nextest and emit JUnit XML at
`target/nextest/<profile>/junit.xml` (`default` locally). CI selects separate
`ci` and `docker-ci` profiles, runs all selected tests even after failures,
and preserves reports as workflow artifacts when tests fail. Doc and browser checks use
separate runners and are not included in these nextest reports.

The reproducible Nix package can be built with `nix build`; use `nix develop`
for the development shell.
