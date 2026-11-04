# Automated validation

CI runs on GitHub-hosted runners. Jobs group the recipes that compile the same
code, so each job builds the workspace once, and run in parallel. Pull requests
run only the jobs whose inputs changed, and drafts only the quick ones: Docker,
Playwright, macOS, Nix, and packaging jobs wait until a pull request is marked
ready.
The `docs` job runs on every pull request. Pushes to main run every job. The
`ci` job summarizes them: it fails if any job failed. Failed checks do not
cancel unrelated jobs.

Rust build caches are scoped to jobs and saved from main only; pull requests
restore them. The Nix job caches the store, including crane's dependency builds
and the dashboard. Pull requests build the combined package; main builds every
package. Dashboard tools are cached by version.

| Capability | Local recipe | CI job |
| --- | --- | --- |
| Rust formatting | `fmt-check` | `checks` |
| Dependency policy and advisories | `deny` | `checks` |
| All-target, all-feature compilation and Clippy, plus wasm32 Clippy of the dashboard, client, and core | `lint` | `rust` |
| Native workspace tests, against the real embedded dashboard | `test` | `rust` |
| Documentation tests | `doc-test` | `rust` |
| Generated API specification, client, and manifest JSON Schema freshness | `openapi-check` | `rust` |
| Core dependency boundaries | `boundary` | `rust` |
| Published crates on the minimum supported Rust, `rust-version` | `cargo +<rust-version> check` | `msrv` |
| Browser compilation | `ui-check`, `check-wasm` | `wasm-ui` |
| Real browser transport tests | `test-wasm` | `wasm-ui` |
| Isolated Engine integration tests | `docker-test` | `docker-integration` |
| Browser end-to-end tests | `test-playwright` | `browser-playwright` |
| CLI on macOS | `validate-cli` | `cli (macOS)` |
| Embedded daemon and CLI release builds | `nix build` | `nix packages` |
| Every published crate packaged and built from its package | `cargo xtask publish --dry-run` | `package` |
| Documentation book build and link check | `docs` | `docs` |

`just` runs the default validation set (`validate`), including a single check of
the generated OpenAPI specification, client, and manifest JSON Schema. Both local
validation and CI compare the committed artifacts without rewriting them. CI
skips `check` and `build`: the stronger all-target Clippy and the Nix package
build cover them. `fmt` and `generate` are editing commands whose results are
checked above, and `setup-playwright` installs the browser test dependencies.
`run`, `ctl`, `daemon`, `daemon-embedded`, `dev`, and `docs-serve` launch
interactive processes and are not finite validation commands. `coverage`
measures the tests without checking a minimum; see
[test coverage](#test-coverage).

## Docker test suites

Controller tests run against `FakeDocker`, an in-memory Docker engine in
`apps/piqueld/src/docker/fake.rs`. `just docker-test` runs
`apps/piqueld/tests/docker_integration.rs` against a real engine: a throwaway
Docker-in-Docker daemon, never the host's.

The scenarios in `apps/piqueld/tests/support/docker_contract.rs` check that the
fake behaves like the real engine. Both suites run each of them:
`fake_docker/main.rs` against the fake, in the `rust` job, and
`docker_integration.rs` against the engine, in the `docker-integration` job.
When the controller starts relying on another engine behavior, add a scenario
for it.

When a scenario passes against the fake but fails against the engine, check
what Docker actually does. Then fix whichever is wrong: the fake, the adapter,
or the scenario.

## Test coverage

See which code the native workspace tests run:

```console
just coverage
```

The recipe runs the tests `just test` runs under
[cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov). It writes an HTML
report to `target/llvm-cov/html/index.html`, which marks every line of each
source file as run or not. Then it prints each crate's covered lines and
functions.

Arguments go on to nextest. On a busy machine, timing-sensitive tests can
fail, so run fewer at once and retry the ones that fail:

```console
just coverage --test-threads 8 --retries 2
```

`nix develop` provides cargo-llvm-cov, and `rust-toolchain.toml` adds the
`llvm-tools` it reads the results with. Without Nix, install it with
`cargo install --locked cargo-llvm-cov`. The instrumented build goes to
`target/llvm-cov-target`, so it doesn't replace your ordinary one.

CI doesn't run the recipe and enforces no minimum. The numbers leave out the
tests `just test` doesn't run:

- the Docker integration tests, which need an isolated engine
  (`just docker-test`).
- documentation tests (`just doc-test`).
- the browser transport tests (`just test-wasm`) and the Playwright browser
  tests (`just test-playwright`).

`piqueld-ui` counts only the modules that also compile natively, such as its
editor and terminal. Its browser code compiles only for wasm32, which
cargo-llvm-cov can't instrument, so the report doesn't list it.

The totals leave out every directory named `tests`, `examples` or `benches`,
but count other test code: `#[cfg(test)]` modules and test-only files such as
`piqueld-core/src/manifest/template_tests/`. That code always runs, so it
raises a crate's percentage. Stable Rust can't exclude it, so look at the
lines the HTML report marks as not run rather than at the percentages alone.
