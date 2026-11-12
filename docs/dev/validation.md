# Automated validation

CI runs on GitHub-hosted runners. Jobs group the recipes that compile the same
code, so each job builds the workspace once, and run in parallel. Pull requests
run only the jobs whose inputs changed, and drafts only the quick ones: Docker,
Playwright, and macOS jobs wait until a pull request is marked ready. Release
builds take too long to gate a merge, so they run after it, in the
[release checks](#release-checks).
The `docs` job runs on every pull request. Pull requests merge through a merge
queue, which tests each one on top of the latest main; merge queue entries run
every job but macOS, whose scarce runners would hold up the queue. The commit
the queue tested is the one that lands, so pushes to main run no CI. A nightly
run on main runs every job, macOS included; while it fails, an issue titled
*Nightly CI fails on main* stays open, as for the
[release checks](#release-checks). The `ci` job summarizes them: it fails if
any job failed. Failed checks do not cancel unrelated jobs.

Rust build caches are scoped to jobs and saved by the nightly run on main;
pull requests and the merge queue restore them, so after a dependency change
merges they rebuild dependencies until that night. Dashboard tools are cached
by version.

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
| Documentation book build and link check | `docs` | `docs` |

The `rust` and `browser-playwright` jobs embed a dashboard module built once
per version of its sources, instead of compiling it each time; see
[production assets](architecture/dashboard.md#production-assets). The
*Dashboard cache* workflow builds main's module as soon as a merge changes its
sources, so the merge queue and pull requests find it.

`just` runs the default validation set (`validate`), including a single check of
the generated OpenAPI specification, client, and manifest JSON Schema. Both local
validation and CI compare the committed artifacts without rewriting them. CI
skips `check` and `build`: the stronger all-target Clippy and the test builds
cover them. `fmt` and `generate` are editing commands whose results are
checked above, and `setup-playwright` installs the browser test dependencies.
`run`, `ctl`, `daemon`, `daemon-embedded`, `dev`, and `docs-serve` launch
interactive processes and are not finite validation commands. `publish` and
`release` [release piqueld](releasing.md). `coverage`
measures the tests without checking a minimum; see
[test coverage](#test-coverage).

## Release checks

The [release checks workflow](https://github.com/sourceorigin-dev/piqueld/blob/main/.github/workflows/release-checks.yml)
builds piqueld the way it is released. Each build takes up to ten minutes, so
these checks run after a pull request merges instead of before:

| Capability | Local recipe | Job |
| --- | --- | --- |
| Embedded daemon and CLI release builds | `nix build` | `nix packages` |
| Every published crate packaged and built from its package | `publish --dry-run` | `package` |
| The static release archive, installed and then updated by `install.sh` | `dist` | `install` |

They run on each push to `main` that changes what they build from, such as a
manifest, the lockfile, a build script, a migration, the toolchain, the flake,
the dashboard, the examples, `install.sh` or `xtask`. They also run every night at 04:00 UTC,
which catches the rest and what breaks without a commit, such as a new stable
Rust or a new runner image. Only the latest commit is checked: a push cancels
the run of the one before it.

While a check fails on `main`, an issue titled *Release checks fail on main*
stays open. The first failure opens it, each later one comments on it, and the
next run that passes closes it. Fix the failure before the next
[release](releasing.md#release-a-version).

Run a check before merging when a pull request changes what it builds: a
manifest, a build script, `flake.nix`, or `install.sh`. Run its local recipe,
or run the workflow on your branch from the Actions tab, which needs write
access to the repository:

```console
gh workflow run release-checks.yml --ref <branch>
```

The Nix job caches the store, including crane's dependency builds and the
dashboard. The `install` job's Rust cache is also the release workflow's.

## Docker test suites

Controller tests run against `FakeDocker`, an in-memory Docker engine in
`apps/piqueld/src/docker/fake.rs`. `just docker-test` runs two suites against
a real engine: a throwaway Docker-in-Docker daemon, never the host's.

- `ingress` is the daemon library's `ingress_caddy` tests, against Caddy and
  an ACME test server in the engine.
- `engine` is `apps/piqueld/tests/docker_integration.rs`, against the engine's
  Swarm.

The tests of a suite share its engine's Swarm, so they run one at a time. Name
a suite to run only it, and add nextest's `--partition` to run a shard of it:

```console
just docker-test engine --partition count:1/2
```

CI runs three shards side by side, each against an engine of its own: the
`ingress` suite, and each half of the `engine` suite.

The scenarios in `apps/piqueld/tests/support/docker_contract.rs` check that the
fake behaves like the real engine. Two test binaries run each of them:
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
